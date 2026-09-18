#![cfg_attr(rustfmt, rustfmt::skip)]
//! This module takes [`Effect`] values produced by [`super::dispatch`] and spawns them as async tasks on a [`JoinSet`].
//! When tasks complete, the event loop converts their output into [`TaskResult`] and feeds it back through dispatch.
mod helpers;
mod session_list;
use super::actions;
use super::worktree_session;
#[allow(unused_imports)]
use super::{agent, dispatch};
pub use helpers::CompactError;
pub(crate) use helpers::timed_out_while;
pub use session_list::ConversationsPartial;
pub(super) use helpers::{
    parse_session_load_running_prompt_id, parse_session_memory_mode,
};
pub(crate) use helpers::{
    EffectMeta, RestoreProgressMsg, SessionFlags, acp_send_bounded, compact_error,
    is_disk_full_error, parse_worktree_restore_payload, parse_worktree_strategy_summary,
    persist_permission_mode_and_notify, persist_setting, sanitize_user_error,
};
#[cfg(feature = "local-workspace")]
pub(crate) use helpers::reject_non_fs_only_advertised_tools;
use helpers::*;
use session_list::{
    LocalPresence, parse_session_list_partial, parse_session_list_scope,
    parse_session_picker_entries_blocking, read_session_list_response,
    session_picker_entry_to_roster,
};
use std::path::Path;
use agent_client_protocol as acp;
use tokio::task::JoinSet;
use xai_acp_lib::{AcpAgentTx, acp_send};
use xai_grok_telemetry::startup::{self, StartupPhase};
use actions::{
    ClipboardPasteTarget, Effect, SubagentKillOutcome, SwitchModelError, TaskResult,
    WorkspaceMutation, WorkspaceMutationFailure, WorkspaceWriteCompletion,
};
use actions::PermissionModeKind;
use crate::app::session_startup::stamp_span_traceparent;
use crate::views::usage_modal::SessionInfoField;
#[cfg(test)]
use actions::PermissionModePersist;
#[cfg(test)]
use agent::AgentId;
use crate::unified_log as ulog;
use xai_grok_shell::sampling::error::http_status_from_error;
use xai_grok_shell::session::{ExtMethodResult, SessionInfoResponse};
/// The shell's `x.ai/feedback/upload-trace` params. `intent` is omitted (not null) when absent, so a legacy upload's request stays byte-identical to the pre-intent shape.
/// absent, so a legacy upload's request stays byte-identical to the pre-intent shape.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadTraceRequest {
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    intent: Option<crate::views::feedback_modal::FeedbackTraceUploadIntent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_upload_token: Option<String>,
}
const SESSION_LIST_LIMIT: u64 = 30;
const DASHBOARD_SESSION_LIST_LIMIT: u64 = 100;
/// MCP discovery reads and parses several config sources (global and project, from `cwd` up to the repository root),
/// so it runs on the blocking pool rather than the UI thread or a tokio worker.
/// Session-open paths have no resolved per-vendor compat in scope; the default (all-on) preserves existing behavior.
pub(crate) async fn discover_mcp_servers(
    cwd: std::path::PathBuf,
) -> Vec<acp::McpServer> {
    let started = std::time::Instant::now();
    let servers = tokio::task::spawn_blocking(move || xai_grok_shell::util::config::load_mcp_servers(
            &cwd,
            &xai_grok_tools::types::compat::CompatConfig::default(),
        ))
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "mcp server discovery task failed");
            Vec::new()
        });
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis() as u64,
        server_count = servers.len(),
        "mcp server discovery"
    );
    servers
}
fn apply_permission_mode_override(
    meta: &mut Option<acp::Meta>,
    permission_mode_override: Option<PermissionModeKind>,
) {
    let Some(mode) = permission_mode_override else {
        return;
    };
    let meta = meta.get_or_insert_with(acp::Meta::new);
    meta.insert("yoloMode".into(), serde_json::Value::Bool(mode.is_always_approve()));
    meta.insert("autoMode".into(), serde_json::Value::Bool(mode.is_auto()));
}
/// Send `session/new` inside the `session_create.backend_rpc` region and stamp that region's
/// traceparent, so the agent-side leg nests under this round-trip, not the enclosing phase span.
async fn create_session_in_backend_rpc(
    request: acp::NewSessionRequest,
    mut meta: Option<acp::Meta>,
    tx: &AcpAgentTx,
    action: &str,
) -> Result<acp::NewSessionResponse, helpers::SessionRpcError> {
    let rpc_span = match startup::current_phase_span().as_ref() {
        Some(parent) => {
            xai_grok_telemetry::region!(
            "startup.session_create.backend_rpc",
            xai_grok_telemetry::region::Parent::Explicit(parent)
        )
        }
        None => {
            xai_grok_telemetry::region!(
            "session.create.backend_rpc",
            xai_grok_telemetry::region::Parent::Inherit
        )
        }
    };
    stamp_span_traceparent(&mut meta, rpc_span.span());
    helpers::acp_send_bounded(request.meta(meta), tx, action).await
}
pub(crate) fn execute(
    effect: Effect,
    tasks: &mut JoinSet<TaskResult>,
    acp_tx: &AcpAgentTx,
    cwd: &Path,
    session_flags: &SessionFlags,
    progress_tx: &tokio::sync::mpsc::UnboundedSender<RestoreProgressMsg>,
) -> (bool, EffectMeta) {
    let mut meta = EffectMeta::default();
    let effect_is_send_now = matches!(effect, Effect::SendPromptNow { .. });
    match effect {
        Effect::RegisterActiveSession { session_id, cwd } => {
            crate::app::signal_handler::set_current_session_id(Some(session_id.clone()));
            if let Err(e) = xai_grok_active_sessions::register(xai_grok_active_sessions::ActiveSession {
                session_id,
                pid: std::process::id(),
                cwd,
                opened_at: chrono::Utc::now(),
            }) {
                tracing::warn!(?e, "Failed to register active session");
            }
        }
        Effect::UnregisterActiveSession { session_id } => {
            crate::app::signal_handler::clear_current_session_id_if(&session_id);
            unregister_active_session_best_effort(&session_id);
        }
        Effect::Quit => {
            ulog::info("pager quit", None, None);
            return (true, meta);
        }
        Effect::ResetMouseReporting => {}
        Effect::SetWorkingDir { path } => {
            if let Err(e) = std::env::set_current_dir(&path) {
                tracing::warn!(error = %e, "change location: failed to set_current_dir");
            }
        }
        Effect::RunStatusLineCommand(run) => {
            tasks
                .spawn(async move {
                    let (id, outcome) = run.execute().await;
                    TaskResult::StatusLineCommandFinished {
                        id,
                        outcome,
                    }
                });
        }
        Effect::ScheduleClearAuthCopyFeedback { generation } => {
            tasks
                .spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    TaskResult::AuthCopyFeedbackTimeout {
                        generation,
                    }
                });
        }
        Effect::Logout => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    send_logout(&tx).await;
                    TaskResult::LogoutComplete
                });
        }
        Effect::CancelAuth { request_seq } => {
            let tx = acp_tx.clone();
            tasks.spawn(async move { send_auth_cancel(&tx, request_seq).await });
        }
        Effect::CheckSubscription { verify } => {
            let tx = acp_tx.clone();
            tasks.spawn(async move { send_check_subscription(&tx, verify).await });
        }
        Effect::CreditLimitRecheck { agent_id } => {
            let tx = acp_tx.clone();
            tasks.spawn(async move { send_credit_limit_recheck(&tx, agent_id).await });
        }
        Effect::SchedulePaywallCheck => {
            tasks
                .spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    TaskResult::PaywallCheckTick
                });
        }
        Effect::ScheduleGateVerifyTimeout { generation } => {
            tasks
                .spawn(async move {
                    tokio::time::sleep(crate::app::subscription::GATE_VERIFY_TIMEOUT)
                        .await;
                    TaskResult::GateVerifyTimeout {
                        generation,
                    }
                });
        }
        Effect::SwitchAccount { request_seq, method_id, use_oauth } => {
            let tx = acp_tx.clone();
            let abort_handle = tasks
                .spawn(async move {
                    send_logout(&tx).await;
                    send_authenticate(&tx, request_seq, method_id, use_oauth, false)
                        .await
                });
            meta.auth_abort_handle = Some((request_seq, abort_handle));
        }
        Effect::CreateSession {
            agent_id,
            cwd: session_cwd,
            model_id,
            permission_mode_override,
            preferred_session_id,
            chat_kind,
        } => {
            let tx = acp_tx.clone();
            #[allow(unused_mut)]
            let mut meta = session_flags.to_meta();
            apply_permission_mode_override(&mut meta, permission_mode_override);
            let is_chat_path = chat_kind || session_flags.chat_mode;
            finalize_chat_session_meta(&mut meta, is_chat_path, session_flags);
            if let Some(ref mid) = model_id {
                meta.get_or_insert_with(acp::Meta::new)
                    .insert("modelId".into(), serde_json::json!(mid.0));
            }
            if let Some(ref sid) = preferred_session_id {
                meta.get_or_insert_with(acp::Meta::new)
                    .insert("sessionId".into(), serde_json::json!(sid));
            }
            if is_chat_path {
                scrub_chat_workspace_bind_meta(&mut meta);
            }
            let preferred_for_preflight = preferred_session_id.clone();
            tasks
                .spawn(async move {
                    if let Some(ref sid) = preferred_for_preflight {
                        let session_cwd_str = session_cwd.to_string_lossy();
                        if let Err(e) = crate::app::session_startup::ensure_session_id_available(
                            sid,
                            &session_cwd_str,
                        ) {
                            return TaskResult::SessionFailed {
                                agent_id,
                                error: sanitize_user_error(&e.to_string()),
                                timed_out: false,
                            };
                        }
                    }
                    let mcp_servers = discover_mcp_servers(session_cwd.clone()).await;
                    let mcp_count = mcp_servers.len();
                    let _phase = startup::phase_scope(StartupPhase::SessionCreate);
                    ulog::info(
                        "session.create.start",
                        None,
                        Some(serde_json::json!({"mcp_server_count": mcp_count})),
                    );
                    let create_start = std::time::Instant::now();
                    let result = create_session_in_backend_rpc(
                            acp::NewSessionRequest::new(session_cwd)
                                .mcp_servers(mcp_servers),
                            meta,
                            &tx,
                            "Session creation",
                        )
                        .await;
                    let create_elapsed_ms = create_start.elapsed().as_millis() as u64;
                    match result {
                        Ok(resp) => {
                            ulog::info(
                                "session.create.done",
                                Some(&resp.session_id.0),
                                Some(
                                    serde_json::json!({
                                "elapsed_ms": create_elapsed_ms,
                                "mcp_server_count": mcp_count,
                            }),
                                ),
                            );
                            TaskResult::WithPinnedMemoryMode {
                                agent_id,
                                memory_mode: parse_session_memory_mode(resp.meta.as_ref()),
                                result: Box::new(TaskResult::SessionCreated {
                                    agent_id,
                                    session_id: resp.session_id,
                                    models: resp.models,
                                    modes: resp.modes,
                                }),
                            }
                        }
                        Err(e) => {
                            let timed_out = e.timed_out();
                            let error = e.to_string();
                            ulog::error(
                                "session.create.failed",
                                None,
                                Some(
                                    serde_json::json!({
                                "elapsed_ms": create_elapsed_ms,
                                "error": &error,
                            }),
                                ),
                            );
                            TaskResult::SessionFailed {
                                agent_id,
                                error: sanitize_user_error(&error),
                                timed_out,
                            }
                        }
                    }
                });
        }
        Effect::CreateWorktreeSession {
            agent_id,
            load_session_id,
            label,
            git_ref,
            model_id,
            permission_mode_override,
            preferred_session_id,
            minted_session_id,
            chat_kind,
        } => {
            let tx = acp_tx.clone();
            let cwd = cwd.to_path_buf();
            let mut meta = session_flags.to_meta();
            apply_permission_mode_override(&mut meta, permission_mode_override);
            finalize_chat_session_meta(
                &mut meta,
                chat_kind || session_flags.chat_mode,
                session_flags,
            );
            if let Some(ref mid) = model_id {
                meta.get_or_insert_with(acp::Meta::new)
                    .insert("modelId".into(), serde_json::json!(mid.0));
            }
            let client_session_id = load_session_id
                .is_none()
                .then(|| preferred_session_id.clone().or(minted_session_id))
                .flatten();
            if let Some(ref sid) = client_session_id {
                meta.get_or_insert_with(acp::Meta::new)
                    .insert("sessionId".into(), serde_json::json!(sid));
            }
            let restore_code = session_flags.restore_code;
            let resume_local_miss = session_flags.resume_local_miss.clone();
            tracing::info!(
                ?restore_code,
                ?load_session_id,
                ?git_ref,
                "CreateWorktreeSession: restore_code, load_session_id, git_ref"
            );
            let spec = worktree_session::WorktreeSpec {
                label,
                git_ref,
            };
            tasks
                .spawn(async move {
                    if let Some(sid) = load_session_id {
                        let local_miss = resume_local_miss
                            .as_deref()
                            .filter(|t| *t == sid);
                        let _phase = startup::phase_scope(StartupPhase::SessionCreate);
                        return match worktree_session::resume_session_into_worktree(
                                &tx,
                                &cwd,
                                &spec,
                                &sid,
                                restore_code,
                                local_miss,
                            )
                            .await
                        {
                            Ok(resumed) => {
                                TaskResult::WorktreeForked {
                                    agent_id,
                                    session_id: acp::SessionId::new(resumed.session_id),
                                    worktree_path: resumed.worktree_root,
                                    session_cwd: resumed.session_cwd,
                                    code_restored: resumed.code_restored,
                                    restore_summary: resumed.restore_summary,
                                    restore_degree: resumed.restore_degree,
                                    resume_session_id: Some(sid),
                                    strategy_summary: resumed.strategy_summary,
                                }
                            }
                            Err(e) => {
                                TaskResult::WorktreeSessionFailed {
                                    agent_id,
                                    error: e.message,
                                    orphaned_worktree_root: None,
                                    timed_out: false,
                                }
                            }
                        };
                    }
                    let worktree_id = worktree_session::new_worktree_id(
                        preferred_session_id.as_deref(),
                    );
                    let created = match worktree_session::create_worktree(
                            &tx,
                            &cwd,
                            &spec,
                            &worktree_id,
                        )
                        .await
                    {
                        Ok(created) => created,
                        Err(e) => {
                            return TaskResult::WorktreeSessionFailed {
                                agent_id,
                                error: e.message,
                                orphaned_worktree_root: None,
                                timed_out: false,
                            };
                        }
                    };
                    let worktree_root = created.worktree_root;
                    let session_cwd = created.session_cwd;
                    let strategy_summary = created.strategy_summary;
                    if let Some(ref sid) = client_session_id {
                        let session_cwd_str = session_cwd.to_string_lossy();
                        if let Err(e) = crate::app::session_startup::ensure_session_id_available(
                            sid,
                            &session_cwd_str,
                        ) {
                            return TaskResult::WorktreeSessionFailed {
                                agent_id,
                                error: sanitize_user_error(&e.to_string()),
                                orphaned_worktree_root: Some(worktree_root),
                                timed_out: false,
                            };
                        }
                    }
                    let mcp_servers = discover_mcp_servers(session_cwd.clone()).await;
                    let _phase = startup::phase_scope(StartupPhase::SessionCreate);
                    let result = create_session_in_backend_rpc(
                            acp::NewSessionRequest::new(session_cwd.clone())
                                .mcp_servers(mcp_servers),
                            meta,
                            &tx,
                            "Worktree session creation",
                        )
                        .await;
                    match result {
                        Ok(resp) => {
                            TaskResult::WithPinnedMemoryMode {
                                agent_id,
                                memory_mode: parse_session_memory_mode(resp.meta.as_ref()),
                                result: Box::new(TaskResult::WorktreeSessionCreated {
                                    agent_id,
                                    session_id: resp.session_id,
                                    worktree_path: worktree_root,
                                    session_cwd,
                                    models: resp.models,
                                    modes: resp.modes,
                                    strategy_summary,
                                }),
                            }
                        }
                        Err(e) => {
                            TaskResult::WorktreeSessionFailed {
                                agent_id,
                                error: sanitize_user_error(
                                    &format!("couldn't create session in worktree: {e}"),
                                ),
                                orphaned_worktree_root: Some(worktree_root),
                                timed_out: e.timed_out(),
                            }
                        }
                    }
                });
        }
        Effect::LoadSession { agent_id, session_id, session_cwd, chat_kind } => {
            let tx = acp_tx.clone();
            let mut meta = session_flags.to_meta();
            let is_chat_path = chat_kind || session_flags.chat_mode;
            finalize_chat_session_meta(&mut meta, is_chat_path, session_flags);
            if let Some(rc) = session_flags.restore_code {
                meta.get_or_insert_with(acp::Meta::new)
                    .insert("x.ai/restore_code".into(), serde_json::Value::Bool(rc));
            }
            let cwd = session_cwd.unwrap_or_else(|| cwd.to_path_buf());
            let acp_session_id = acp::SessionId::new(session_id);
            tasks
                .spawn(async move {
                    let mcp_servers = discover_mcp_servers(cwd.clone()).await;
                    let _phase = startup::phase_scope(StartupPhase::SessionCreate);
                    ulog::info("session.load.start", Some(&acp_session_id.0), None);
                    let load_started = std::time::Instant::now();
                    let result = helpers::acp_send_bounded(
                            acp::LoadSessionRequest::new(acp_session_id.clone(), cwd)
                                .mcp_servers(mcp_servers)
                                .meta(meta),
                            &tx,
                            "Session loading",
                        )
                        .await;
                    let load_elapsed_ms = load_started.elapsed().as_millis() as u64;
                    tracing::info!(
                    session_id = %acp_session_id.0,
                    elapsed_ms = load_elapsed_ms,
                    ok = result.is_ok(),
                    "load_session: acp load_session completed"
                );
                    match result {
                        Ok(resp) => {
                            ulog::info(
                                "session.load.done",
                                Some(&acp_session_id.0),
                                Some(serde_json::json!({"elapsed_ms": load_elapsed_ms})),
                            );
                            let (code_restored, restore_summary, restore_degree) = parse_session_load_restore_meta(
                                resp.meta.as_ref(),
                            );
                            let running_prompt_id = parse_session_load_running_prompt_id(
                                resp.meta.as_ref(),
                            );
                            TaskResult::WithPinnedMemoryMode {
                                agent_id,
                                memory_mode: parse_session_memory_mode(resp.meta.as_ref()),
                                result: Box::new(TaskResult::SessionLoaded {
                                    agent_id,
                                    session_id: acp_session_id,
                                    models: resp.models,
                                    modes: resp.modes,
                                    code_restored,
                                    restore_summary,
                                    restore_degree,
                                    running_prompt_id,
                                }),
                            }
                        }
                        Err(e) => {
                            let error = e.to_string();
                            ulog::error(
                                "session.load.failed",
                                Some(&acp_session_id.0),
                                Some(
                                    serde_json::json!({"elapsed_ms": load_elapsed_ms, "error": &error}),
                                ),
                            );
                            TaskResult::SessionLoadFailed {
                                agent_id,
                                session_id: acp_session_id,
                                error: sanitize_user_error(&error),
                            }
                        }
                    }
                });
        }
        Effect::ScanForeignSessions { cwd, compat, grok_home, coordinator, seq } => {
            if coordinator.latest_seq() != seq {
                return (false, meta);
            }
            let semaphore = coordinator.semaphore();
            let latest_seq = coordinator.latest_seq_handle();
            let abort_handle = tasks
                .spawn(async move {
                    let Ok(permit) = semaphore.acquire_owned().await else {
                        return TaskResult::ForeignSessionsScanned {
                            entries: Vec::new(),
                            seq,
                        };
                    };
                    if latest_seq.load(std::sync::atomic::Ordering::Acquire) != seq {
                        return TaskResult::ForeignSessionsScanned {
                            entries: Vec::new(),
                            seq,
                        };
                    }
                    let enabled = crate::app::foreign_sessions::gated_sources_async(
                            compat,
                            &grok_home,
                        )
                        .await;
                    if latest_seq.load(std::sync::atomic::Ordering::Acquire) != seq
                        || !(enabled.claude || enabled.codex || enabled.cursor)
                    {
                        return TaskResult::ForeignSessionsScanned {
                            entries: Vec::new(),
                            seq,
                        };
                    }
                    let summaries = tokio::task::spawn_blocking(move || {
                            let _permit = permit;
                            xai_grok_foreign_sessions::scan_foreign_sessions(
                                &cwd,
                                enabled,
                            )
                        })
                        .await
                        .unwrap_or_else(|error| {
                            tracing::warn!(%error, "foreign session scan task failed");
                            Vec::new()
                        });
                    let entries = summaries
                        .into_iter()
                        .map(crate::app::foreign_sessions::map_summary)
                        .collect();
                    TaskResult::ForeignSessionsScanned {
                        entries,
                        seq,
                    }
                });
            coordinator.install_abort_handle(seq, abort_handle);
        }
        Effect::CanonicalizeForeignResumeCwd { requested_cwd, launch_token } => {
            tasks
                .spawn(async move {
                    let cwd_for_task = requested_cwd.clone();
                    let canonical_cwd = tokio::task::spawn_blocking(move || {
                            dunce::canonicalize(cwd_for_task).ok()
                        })
                        .await
                        .unwrap_or_else(|error| {
                            tracing::warn!(%error, "foreign resume cwd canonicalization task failed");
                            None
                        });
                    TaskResult::ForeignResumeCwdCanonicalized {
                        requested_cwd,
                        canonical_cwd,
                        launch_token,
                    }
                });
        }
        Effect::DetectForeignResumeHint {
            canonical_cwd,
            compat,
            grok_home,
            launch_token,
        } => {
            tasks
                .spawn(async move {
                    let cwd_for_scan = canonical_cwd.clone();
                    let recent = crate::app::foreign_sessions::with_gated_sources_async(
                            compat,
                            &grok_home,
                            |enabled| async move {
                                tokio::task::spawn_blocking(move || xai_grok_foreign_sessions::most_recent_foreign_session(
                                        &cwd_for_scan,
                                        enabled,
                                        crate::app::foreign_sessions::RESUME_HINT_WINDOW,
                                    ))
                                    .await
                                    .unwrap_or_else(|error| {
                                        tracing::warn!(%error, "foreign resume detection task failed");
                                        None
                                    })
                            },
                        )
                        .await
                        .flatten();
                    TaskResult::ForeignResumeHintDetected {
                        canonical_cwd,
                        launch_token,
                        hint: recent,
                    }
                });
        }
        Effect::FetchSessionList {
            host,
            cwd_override,
            generation,
            query,
            seq,
            kind_filter,
            headless_policy,
        } => {
            let tx = acp_tx.clone();
            let cwd = cwd_override.unwrap_or_else(|| cwd.to_path_buf());
            tasks
                .spawn(async move {
                    let limit = if host
                        == crate::views::session_picker_surface::SessionPickerHost::Dashboard
                    {
                        DASHBOARD_SESSION_LIST_LIMIT
                    } else {
                        SESSION_LIST_LIMIT
                    };
                    let mut params = serde_json::json!({
                    "cwd": cwd.to_string_lossy(),
                    "limit": limit,
                    "headless": headless_policy.as_wire_str(),
                });
                    if let Some(obj) = params.as_object_mut() {
                        if let Some(q) = &query {
                            obj.insert(
                                "query".into(),
                                serde_json::Value::String(q.clone()),
                            );
                        } else {
                            obj.insert(
                                "allowRelax".into(),
                                serde_json::Value::Bool(true),
                            );
                        }
                        if let Some(kinds) = &kind_filter {
                            obj.insert(
                                "_meta".into(),
                                serde_json::json!({
                                "x.ai/facetFilters": { "kind": kinds },
                            }),
                            );
                            tracing::info!(
                            target: "grok.pager.workspace_mode",
                            event = "session_list_fetch",
                            kind_filter = ?kinds,
                            query = ?query,
                            ?host,
                            generation,
                            seq,
                            "FetchSessionList with kind facet filter"
                        );
                        }
                    }
                    let request = acp::ExtRequest::new(
                        "x.ai/session/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize session list params")
                            .into(),
                    );
                    let result = acp_send(request, &tx).await;
                    match result {
                        Ok(resp) => {
                            let payload = match read_session_list_response(
                                resp.0.get(),
                            ) {
                                Ok(payload) => payload,
                                Err(error) => {
                                    return TaskResult::SessionListFailed {
                                        host,
                                        generation,
                                        error,
                                        seq,
                                        query,
                                    };
                                }
                            };
                            let partial = parse_session_list_partial(&payload);
                            let scope = parse_session_list_scope(&payload);
                            let sessions = match parse_session_picker_entries_blocking(
                                    payload,
                                    LocalPresence::for_host(host),
                                )
                                .await
                            {
                                Ok(sessions) => sessions,
                                Err(error) => {
                                    return TaskResult::SessionListFailed {
                                        host,
                                        generation,
                                        error: sanitize_user_error(&error),
                                        seq,
                                        query,
                                    };
                                }
                            };
                            TaskResult::SessionListLoaded {
                                host,
                                generation,
                                sessions,
                                partial,
                                scope,
                                seq,
                                query,
                            }
                        }
                        Err(e) => {
                            TaskResult::SessionListFailed {
                                host,
                                generation,
                                error: sanitize_user_error(&format!("{e}")),
                                seq,
                                query,
                            }
                        }
                    }
                });
        }
        Effect::DebounceSessionSearch { host, generation, query, seq } => {
            tasks
                .spawn(async move {
                    tokio::time::sleep(
                            std::time::Duration::from_millis(SESSION_SEARCH_DEBOUNCE_MS),
                        )
                        .await;
                    TaskResult::SessionSearchDebounceExpired {
                        host,
                        generation,
                        query,
                        seq,
                    }
                });
        }
        Effect::FetchRoster => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let request = acp::ExtRequest::new(
                        "x.ai/sessions/list",
                        serde_json::value::to_raw_value(&serde_json::json!({}))
                            .expect("serialize roster list params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let parsed = crate::app::roster::parse_roster_list_response(
                                resp.0.get(),
                            );
                            match parsed {
                                Some(r) => {
                                    TaskResult::RosterLoaded {
                                        sessions: r.sessions,
                                    }
                                }
                                None => {
                                    tracing::warn!("failed to parse x.ai/sessions/list response");
                                    TaskResult::RosterFailed {
                                        error: "parse error".to_string(),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            TaskResult::RosterFailed {
                                error: sanitize_user_error(&format!("{e}")),
                            }
                        }
                    }
                });
        }
        Effect::FetchDashboardSessions => {
            let tx = acp_tx.clone();
            let cwd = cwd.to_path_buf();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "cwd": cwd.to_string_lossy(),
                    "limit": 30,
                    "headless": xai_grok_shell::session::unified_list::HeadlessPolicy::Exclude
                        .as_wire_str(),
                });
                    let request = acp::ExtRequest::new(
                        "x.ai/session/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize session list params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let Ok(payload) = read_session_list_response(resp.0.get())
                            else {
                                return TaskResult::DashboardSessionsLoaded {
                                    sessions: vec![],
                                };
                            };
                            let sessions = match parse_session_picker_entries_blocking(
                                    payload,
                                    LocalPresence::Relabel,
                                )
                                .await
                            {
                                Ok(entries) => {
                                    entries
                                        .into_iter()
                                        .map(session_picker_entry_to_roster)
                                        .collect()
                                }
                                Err(error) => {
                                    tracing::warn!(%error, "dashboard session list parse failed");
                                    Vec::new()
                                }
                            };
                            TaskResult::DashboardSessionsLoaded {
                                sessions,
                            }
                        }
                        Err(_) => {
                            TaskResult::DashboardSessionsLoaded {
                                sessions: vec![],
                            }
                        }
                    }
                });
        }
        Effect::LoadWorkspaceSnapshot { db_path } => {
            tasks
                .spawn(async move {
                    match tokio::task::spawn_blocking(move || {
                            let store = xai_grok_dashboard_store::WorkspaceStore::open(
                                &db_path,
                            )?;
                            let snapshot = store.snapshot()?;
                            Ok::<
                                _,
                                xai_grok_dashboard_store::StoreError,
                            >((store, snapshot))
                        })
                        .await
                    {
                        Ok(Ok((store, snapshot))) => {
                            TaskResult::WorkspaceSnapshotLoaded {
                                store,
                                snapshot,
                            }
                        }
                        Ok(Err(error)) => {
                            let retryable = matches!(&error, xai_grok_dashboard_store::StoreError::Busy { .. });
                            TaskResult::WorkspaceSnapshotFailed {
                                error: error.to_string(),
                                retryable,
                            }
                        }
                        Err(error) => {
                            TaskResult::WorkspaceSnapshotFailed {
                                error: format!("workspace loader task failed: {error}"),
                                retryable: false,
                            }
                        }
                    }
                });
        }
        Effect::WriteWorkspace { store, mutation } => {
            let db_path = store.path().to_path_buf();
            tasks
                .spawn(async move {
                    match tokio::task::spawn_blocking(move || {
                            let mut store = store;
                            let mut failures = Vec::new();
                            let completion = match mutation {
                                WorkspaceMutation::Upsert(members) => {
                                    for member in members.iter().cloned() {
                                        let key = member.key.clone();
                                        if let Err(error) = store.insert_member(member) {
                                            failures
                                                .push(WorkspaceMutationFailure {
                                                    key,
                                                    error: error.to_string(),
                                                    retryable: matches!(
                                            &error,
                                            xai_grok_dashboard_store::StoreError::Busy { .. }
                                        ),
                                                });
                                        }
                                    }
                                    WorkspaceWriteCompletion::Upsert {
                                        members,
                                        snapshot: store
                                            .snapshot()
                                            .map_err(|error| error.to_string()),
                                        failures,
                                    }
                                }
                                WorkspaceMutation::Remove(keys) => {
                                    for key in &keys {
                                        if let Err(error) = store.remove_member(key) {
                                            failures
                                                .push(WorkspaceMutationFailure {
                                                    key: key.clone(),
                                                    error: error.to_string(),
                                                    retryable: matches!(
                                            &error,
                                            xai_grok_dashboard_store::StoreError::Busy { .. }
                                        ),
                                                });
                                        }
                                    }
                                    WorkspaceWriteCompletion::Remove {
                                        keys,
                                        snapshot: store
                                            .snapshot()
                                            .map_err(|error| error.to_string()),
                                        failures,
                                    }
                                }
                                WorkspaceMutation::Layout(patch) => {
                                    let outcome = store.apply_layout_patch(&patch);
                                    WorkspaceWriteCompletion::Layout {
                                        patch,
                                        outcome,
                                    }
                                }
                            };
                            (store, completion)
                        })
                        .await
                    {
                        Ok((store, completion)) => {
                            TaskResult::WorkspaceWriteCompleted {
                                store,
                                completion,
                            }
                        }
                        Err(error) => {
                            TaskResult::WorkspaceWriteTaskFailed {
                                db_path,
                                error: format!("workspace writer task failed: {error}"),
                            }
                        }
                    }
                });
        }
        Effect::RefreshWorkspace { store, known_data_version } => {
            let db_path = store.path().to_path_buf();
            tasks
                .spawn(async move {
                    match tokio::task::spawn_blocking(move || {
                            let result = match store.data_version() {
                                Ok(data_version) if data_version == known_data_version => {
                                    Ok(None)
                                }
                                Ok(_) => {
                                    store
                                        .snapshot()
                                        .map(Some)
                                        .map_err(|error| error.to_string())
                                }
                                Err(error) => Err(error.to_string()),
                            };
                            (store, result)
                        })
                        .await
                    {
                        Ok((store, snapshot)) => {
                            TaskResult::WorkspaceRefreshed {
                                store,
                                snapshot,
                            }
                        }
                        Err(error) => {
                            TaskResult::WorkspaceRefreshTaskFailed {
                                db_path,
                                error: format!("workspace refresh task failed: {error}"),
                            }
                        }
                    }
                });
        }
        Effect::RestoreAndLoadSession { agent_id, session_id, session_cwd: _ } => {
            use xai_grok_shell::agent::session_registry_client::SessionRegistryClient;
            use xai_grok_shell::session::restore::restore_session_with_storage;
            let setup_started = std::time::Instant::now();
            let raw_config = xai_grok_shell::config::load_effective_config();
            let setup = raw_config
                .ok()
                .and_then(|raw| {
                    let cfg = xai_grok_shell::agent::config::Config::new_from_toml_cfg(
                            &raw,
                        )
                        .ok()?;
                    let proxy_base = cfg.endpoints.proxy_url();
                    let deployment_key = cfg.endpoints.deployment_key.clone();
                    let alpha_test_key = cfg.endpoints.alpha_test_key.clone();
                    let auth_manager = crate::app::session_startup::pre_acp_auth_manager(
                        &cfg,
                    );
                    let registry = SessionRegistryClient::new(&proxy_base, String::new())
                        .with_deployment_key(deployment_key.clone())
                        .with_alpha_test_key(alpha_test_key.clone())
                        .with_session_id(session_id.clone())
                        .with_auth(auth_manager.clone());
                    let storage = xai_grok_shell::credential_factory::build_storage_client_for_proxy(
                        &proxy_base,
                        deployment_key,
                        alpha_test_key,
                        Some(auth_manager.clone()),
                        None,
                        Some(session_id.clone()),
                        "grok-pager",
                    );
                    Some((auth_manager, registry, storage))
                });
            tracing::info!(
                elapsed_ms = setup_started.elapsed().as_millis() as u64,
                ok = setup.is_some(),
                "restore: auth/client setup"
            );
            let target_cwd = cwd.to_path_buf();
            let ptx = progress_tx.clone();
            tasks
                .spawn(async move {
                    let Some((auth_manager, registry_client, storage_client)) = setup
                    else {
                        return TaskResult::SessionRestoreFailed {
                            agent_id,
                            error: "Failed to load configuration.".into(),
                        };
                    };
                    let _ = auth_manager.auth().await;
                    let progress: Option<
                        xai_grok_shell::session::restore::ProgressCallback,
                    > = {
                        use xai_grok_shell::session::restore::{PhaseStep, RestorePhase};
                        Some(
                            Box::new(move |event| {
                                let msg = match (event.phase, event.step) {
                                    (RestorePhase::Download, PhaseStep::Start) => {
                                        Some("Downloading session archives...".to_string())
                                    }
                                    (RestorePhase::Download, PhaseStep::End) => {
                                        Some(
                                            format!(
                                "Downloads finished ({}).",
                                format_restore_elapsed(event.elapsed),
                            ),
                                        )
                                    }
                                    (RestorePhase::Codebase, PhaseStep::Start) => {
                                        Some("Restoring code...".to_string())
                                    }
                                    (RestorePhase::Codebase, PhaseStep::End) => {
                                        event
                                            .detail
                                            .as_ref()
                                            .map(|detail| format!("Code restored ({detail})."))
                                    }
                                    (RestorePhase::Memory, PhaseStep::Start) => {
                                        Some("Restoring memory...".to_string())
                                    }
                                    (RestorePhase::SessionState, PhaseStep::Start) => {
                                        Some("Restoring session state...".to_string())
                                    }
                                    (RestorePhase::SessionState, PhaseStep::End) => {
                                        event
                                            .detail
                                            .as_ref()
                                            .map(|detail| format!("Session state restored ({detail})."))
                                    }
                                    (RestorePhase::Finalize, _) => {
                                        let status = if event.incomplete {
                                            "Restore incomplete"
                                        } else {
                                            "Restore complete"
                                        };
                                        Some(
                                            format!(
                                    "{status} ({}).",
                                    crate::views::dock::fmt_elapsed(event.elapsed.as_secs())
                                ),
                                        )
                                    }
                                    _ => None,
                                };
                                if let Some(text) = msg {
                                    let _ = ptx
                                        .send(RestoreProgressMsg {
                                            agent_id,
                                            message: text,
                                        });
                                }
                            }),
                        )
                    };
                    let cwd_str = target_cwd.to_string_lossy().to_string();
                    match restore_session_with_storage(
                            &registry_client,
                            &storage_client,
                            &session_id,
                            &cwd_str,
                            xai_grok_shell::session::restore::RestoreSessionOpts {
                                turn_override: None,
                                progress,
                                restore_code: true,
                            },
                        )
                        .await
                    {
                        Ok(result) => {
                            let effective_id = if result.local_session_id.is_empty() {
                                session_id
                            } else {
                                result.local_session_id
                            };
                            TaskResult::SessionRestored {
                                agent_id,
                                local_session_id: effective_id,
                            }
                        }
                        Err(e) => {
                            TaskResult::SessionRestoreFailed {
                                agent_id,
                                error: format!("{e:#}"),
                            }
                        }
                    }
                });
        }
        Effect::LoadCardDetail { host, generation, source, session_id, cwd, seq } => {
            tasks
                .spawn(async move {
                    use crate::app::app_view::CardDetail;
                    let result_session_id = session_id.clone();
                    let detail = tokio::task::spawn_blocking(move || {
                            let info = xai_grok_shell::session::info::Info {
                                id: acp::SessionId::new(session_id),
                                cwd,
                            };
                            let history_path = xai_grok_shell::session::persistence::session_dir(
                                    &info,
                                )
                                .join("chat_history.jsonl");
                            let first_prompt_preview = extract_first_user_prompt(&info)
                                .unwrap_or_default();
                            let (turn_count, tool_call_count) = count_chat_history_stats(
                                &history_path,
                            );
                            CardDetail {
                                turn_count,
                                tool_call_count,
                                first_prompt_preview,
                            }
                        })
                        .await
                        .unwrap_or(CardDetail {
                            turn_count: 0,
                            tool_call_count: 0,
                            first_prompt_preview: String::new(),
                        });
                    TaskResult::CardDetailLoaded {
                        host,
                        generation,
                        source,
                        session_id: result_session_id,
                        seq,
                        detail,
                    }
                });
        }
        Effect::SendPrompt {
            agent_id,
            session_id,
            text,
            prompt_id,
            skill_token_ranges,
        } => {
            let tx = acp_tx.clone();
            let screen_mode = session_flags.screen_mode_label;
            let is_api_key_auth = session_flags.is_api_key_auth;
            tasks
                .spawn(async move {
                    ulog::info(
                        "prompt.acp_send.start",
                        Some(&session_id.0),
                        Some(
                            serde_json::json!({
                        "kind": "text",
                        "len": text.len(),
                        "prompt_id": prompt_id,
                    }),
                        ),
                    );
                    let send_start = std::time::Instant::now();
                    let prompt = vec![plain_prompt_content_block(text, &skill_token_ranges)];
                    let req = acp::PromptRequest::new(session_id.clone(), prompt)
                        .meta(
                            prompt_request_meta(&prompt_id, screen_mode)
                                .as_object()
                                .cloned(),
                        );
                    let result = acp_send(req, &tx).await;
                    let send_elapsed_ms = send_start.elapsed().as_millis() as u64;
                    ulog::info(
                        "prompt.acp_send.done",
                        Some(&session_id.0),
                        Some(
                            serde_json::json!({
                        "kind": "text",
                        "elapsed_ms": send_elapsed_ms,
                        "ok": result.is_ok(),
                        "prompt_id": prompt_id,
                    }),
                        ),
                    );
                    log_prompt_result(&session_id, &result);
                    let http_status = result
                        .as_ref()
                        .err()
                        .and_then(http_status_from_error);
                    TaskResult::PromptResponse {
                        agent_id,
                        result: result
                            .map_err(|e| format_acp_error(&e, is_api_key_auth)),
                        http_status,
                        prompt_id: Some(prompt_id),
                    }
                });
        }
        Effect::ExecutePlan {
            agent_id,
            session_id,
            prompt_id,
            plan_file_content,
            plan_file_uri,
        } => {
            let tx = acp_tx.clone();
            let screen_mode = session_flags.screen_mode_label;
            let is_api_key_auth = session_flags.is_api_key_auth;
            tasks
                .spawn(async move {
                    let prompt = vec![acp::ContentBlock::Text(acp::TextContent::new(String::new()))];
                    let mut meta = prompt_request_meta(&prompt_id, screen_mode);
                    if let Some(map) = meta.as_object_mut() {
                        let mut execute_plan = serde_json::Map::new();
                        execute_plan
                            .insert(
                                "planFileContent".into(),
                                serde_json::Value::String(plan_file_content),
                            );
                        if let Some(uri) = plan_file_uri
                            .filter(|uri| !uri.trim().is_empty())
                        {
                            execute_plan
                                .insert(
                                    "planFileUri".into(),
                                    serde_json::Value::String(uri),
                                );
                        }
                        map.insert(
                            "executePlan".into(),
                            serde_json::Value::Object(execute_plan),
                        );
                    }
                    let req = acp::PromptRequest::new(session_id.clone(), prompt)
                        .meta(meta.as_object().cloned());
                    let result = acp_send(req, &tx).await;
                    log_prompt_result(&session_id, &result);
                    let http_status = result
                        .as_ref()
                        .err()
                        .and_then(http_status_from_error);
                    TaskResult::PromptResponse {
                        agent_id,
                        result: result
                            .map_err(|e| format_acp_error(&e, is_api_key_auth)),
                        http_status,
                        prompt_id: Some(prompt_id),
                    }
                });
        }
        Effect::SendPromptBlocks { agent_id, session_id, blocks, prompt_id }
        | Effect::SendPromptNow { agent_id, session_id, blocks, prompt_id } => {
            let send_now = effect_is_send_now;
            let tx = acp_tx.clone();
            let screen_mode = session_flags.screen_mode_label;
            let is_api_key_auth = session_flags.is_api_key_auth;
            tasks
                .spawn(async move {
                    ulog::info(
                        "prompt.acp_send.start",
                        Some(&session_id.0),
                        Some(
                            serde_json::json!({
                        "kind": if send_now { "send_now" } else { "blocks" },
                        "block_count": blocks.len(),
                        "prompt_id": prompt_id,
                    }),
                        ),
                    );
                    let send_start = std::time::Instant::now();
                    let mut meta = prompt_request_meta(&prompt_id, screen_mode);
                    if send_now && let Some(map) = meta.as_object_mut() {
                        map.insert("sendNow".into(), serde_json::Value::Bool(true));
                    }
                    let requeue_blocks = send_now.then(|| blocks.clone());
                    let req = acp::PromptRequest::new(session_id.clone(), blocks)
                        .meta(meta.as_object().cloned());
                    let result = acp_send(req, &tx).await;
                    let send_elapsed_ms = send_start.elapsed().as_millis() as u64;
                    ulog::info(
                        "prompt.acp_send.done",
                        Some(&session_id.0),
                        Some(
                            serde_json::json!({
                        "kind": if send_now { "send_now" } else { "blocks" },
                        "elapsed_ms": send_elapsed_ms,
                        "ok": result.is_ok(),
                        "prompt_id": prompt_id,
                    }),
                        ),
                    );
                    log_prompt_result(&session_id, &result);
                    if let (Some(blocks), Err(e)) = (requeue_blocks, &result) {
                        return TaskResult::SendPromptNowFailed {
                            agent_id,
                            session_id,
                            prompt_id,
                            error: format_acp_error(e, is_api_key_auth),
                            blocks,
                        };
                    }
                    let http_status = result
                        .as_ref()
                        .err()
                        .and_then(http_status_from_error);
                    TaskResult::PromptResponse {
                        agent_id,
                        result: result
                            .map_err(|e| format_acp_error(&e, is_api_key_auth)),
                        http_status,
                        prompt_id: Some(prompt_id),
                    }
                });
        }
        Effect::SendBashCommand { agent_id, session_id, command, prompt_id } => {
            let tx = acp_tx.clone();
            let screen_mode = session_flags.screen_mode_label;
            let is_api_key_auth = session_flags.is_api_key_auth;
            tasks
                .spawn(async move {
                    use xai_grok_shell::extensions::prompt_meta::PromptBlockMeta;
                    ulog::info(
                        "prompt.acp_send.start",
                        Some(&session_id.0),
                        Some(
                            serde_json::json!({
                        "kind": "bash",
                        "len": command.len(),
                        "prompt_id": prompt_id,
                    }),
                        ),
                    );
                    let send_start = std::time::Instant::now();
                    let meta = PromptBlockMeta::bash(&command);
                    let prompt = vec![acp::ContentBlock::Text(
                    acp::TextContent::new(command).meta(
                        serde_json::to_value(&meta)
                            .expect("PromptBlockMeta serializes")
                            .as_object()
                            .cloned(),
                    ),
                )];
                    let req = acp::PromptRequest::new(session_id.clone(), prompt)
                        .meta(
                            prompt_request_meta(&prompt_id, screen_mode)
                                .as_object()
                                .cloned(),
                        );
                    let result = acp_send(req, &tx).await;
                    let send_elapsed_ms = send_start.elapsed().as_millis() as u64;
                    ulog::info(
                        "prompt.acp_send.done",
                        Some(&session_id.0),
                        Some(
                            serde_json::json!({
                        "kind": "bash",
                        "elapsed_ms": send_elapsed_ms,
                        "ok": result.is_ok(),
                        "prompt_id": prompt_id,
                    }),
                        ),
                    );
                    log_prompt_result(&session_id, &result);
                    let http_status = result
                        .as_ref()
                        .err()
                        .and_then(http_status_from_error);
                    TaskResult::PromptResponse {
                        agent_id,
                        result: result
                            .map_err(|e| format_acp_error(&e, is_api_key_auth)),
                        http_status,
                        prompt_id: Some(prompt_id),
                    }
                });
        }
        Effect::CancelTurn {
            session_id,
            cancel_subagents,
            trigger,
            rewind_prompt_id,
        } => {
            let tx = acp_tx.clone();
            let trigger_str = trigger.map(|t| t.as_wire_str());
            tasks
                .spawn(async move {
                    ulog::info(
                        "cancel.acp_send.start",
                        Some(&session_id.0),
                        Some(
                            serde_json::json!({
                        "cancel_subagents": cancel_subagents,
                        "trigger": trigger_str,
                        "rewind_if_no_output": rewind_prompt_id.is_some(),
                        "rewind_prompt_id": rewind_prompt_id.as_deref(),
                    }),
                        ),
                    );
                    let send_start = std::time::Instant::now();
                    let req = acp::CancelNotification::new(session_id.clone())
                        .meta(
                            Some(
                                cancel_notification_meta(
                                    cancel_subagents,
                                    trigger_str,
                                    rewind_prompt_id.as_deref(),
                                ),
                            ),
                        );
                    let result = acp_send(req, &tx).await;
                    ulog::info(
                        "cancel.acp_send.done",
                        Some(&session_id.0),
                        Some(
                            serde_json::json!({
                        "ok": result.is_ok(),
                        "elapsed_ms": send_start.elapsed().as_millis() as u64,
                    }),
                        ),
                    );
                    if let Err(e) = result {
                        tracing::warn!("Failed to send cancel notification: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::TogglePlanMode { session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                });
                    let notification = acp::ExtNotification::new(
                        "x.ai/toggle_plan_mode",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize toggle_plan_mode params")
                            .into(),
                    );
                    if let Err(e) = acp_send(notification, &tx).await {
                        tracing::warn!("Failed to send toggle_plan_mode notification: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::QueueRemove { session_id, id, expected_version } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "id": id,
                    "expectedVersion": expected_version,
                });
                    let notification = acp::ExtNotification::new(
                        "x.ai/queue/remove",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize queue/remove params")
                            .into(),
                    );
                    if let Err(e) = acp_send(notification, &tx).await {
                        tracing::warn!("Failed to send queue/remove notification: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::QueueReorder { session_id, ordered_ids } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "orderedIds": ordered_ids,
                });
                    let notification = acp::ExtNotification::new(
                        "x.ai/queue/reorder",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize queue/reorder params")
                            .into(),
                    );
                    if let Err(e) = acp_send(notification, &tx).await {
                        tracing::warn!("Failed to send queue/reorder notification: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::QueueClear { session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                });
                    let notification = acp::ExtNotification::new(
                        "x.ai/queue/clear",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize queue/clear params")
                            .into(),
                    );
                    if let Err(e) = acp_send(notification, &tx).await {
                        tracing::warn!("Failed to send queue/clear notification: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::QueueEdit { session_id, id, new_text } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "id": id,
                    "newText": new_text,
                });
                    let notification = acp::ExtNotification::new(
                        "x.ai/queue/edit",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize queue/edit params")
                            .into(),
                    );
                    if let Err(e) = acp_send(notification, &tx).await {
                        tracing::warn!("Failed to send queue/edit notification: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::QueueHoldEdit { session_id, id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "id": id,
                });
                    let notification = acp::ExtNotification::new(
                        "x.ai/queue/hold_edit",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize queue/hold_edit params")
                            .into(),
                    );
                    if let Err(e) = acp_send(notification, &tx).await {
                        tracing::warn!("Failed to send queue/hold_edit notification: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::QueueReleaseEdit { session_id, id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "id": id,
                });
                    let notification = acp::ExtNotification::new(
                        "x.ai/queue/release_edit",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize queue/release_edit params")
                            .into(),
                    );
                    if let Err(e) = acp_send(notification, &tx).await {
                        tracing::warn!("Failed to send queue/release_edit notification: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::QueueInterject { session_id, id, expected_version, new_text } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let mut params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "id": id,
                    "expectedVersion": expected_version,
                });
                    if let Some(new_text) = new_text
                        && let Some(obj) = params.as_object_mut()
                    {
                        obj.insert(
                            "newText".into(),
                            serde_json::Value::String(new_text),
                        );
                    }
                    let notification = acp::ExtNotification::new(
                        "x.ai/queue/interject",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize queue/interject params")
                            .into(),
                    );
                    if let Err(e) = acp_send(notification, &tx).await {
                        tracing::warn!("Failed to send queue/interject notification: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::SetSessionMode { session_id, mode_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let req = acp::SetSessionModeRequest::new(
                        session_id.clone(),
                        mode_id,
                    );
                    if let Err(e) = acp_send(req, &tx).await {
                        tracing::warn!("Failed to set session mode: {e}");
                        return TaskResult::SetSessionModeFailed {
                            session_id,
                        };
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::SetModeThenPrompt {
            session_id,
            mode_id,
            agent_id,
            text,
            prompt_id,
            skill_token_ranges,
        } => {
            let tx = acp_tx.clone();
            let screen_mode = session_flags.screen_mode_label;
            let is_api_key_auth = session_flags.is_api_key_auth;
            tasks
                .spawn(async move {
                    let mode_req = acp::SetSessionModeRequest::new(
                        session_id.clone(),
                        mode_id,
                    );
                    if let Err(e) = acp_send(mode_req, &tx).await {
                        tracing::warn!("Failed to set session mode: {e}");
                    }
                    ulog::info(
                        "prompt submitted",
                        Some(&session_id.0),
                        Some(serde_json::json!({"len": text.len()})),
                    );
                    let prompt = vec![plain_prompt_content_block(text, &skill_token_ranges)];
                    let req = acp::PromptRequest::new(session_id.clone(), prompt)
                        .meta(
                            prompt_request_meta(&prompt_id, screen_mode)
                                .as_object()
                                .cloned(),
                        );
                    let result = acp_send(req, &tx).await;
                    log_prompt_result(&session_id, &result);
                    let http_status = result
                        .as_ref()
                        .err()
                        .and_then(http_status_from_error);
                    TaskResult::PromptResponse {
                        agent_id,
                        result: result
                            .map_err(|e| format_acp_error(&e, is_api_key_auth)),
                        http_status,
                        prompt_id: Some(prompt_id),
                    }
                });
        }
        Effect::Compact { agent_id, session_id, user_context } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let mut params = serde_json::Map::new();
                    params
                        .insert(
                            "sessionId".into(),
                            serde_json::Value::String(session_id.0.to_string()),
                        );
                    if let Some(ctx) = user_context {
                        params
                            .insert(
                                "userContext".into(),
                                serde_json::Value::String(ctx),
                            );
                    }
                    let params = serde_json::Value::Object(params);
                    let req = acp::ExtRequest::new(
                        "x.ai/compact_conversation",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize compact params")
                            .into(),
                    );
                    let result = acp_send(req, &tx).await;
                    TaskResult::CompactComplete {
                        agent_id,
                        result: result.map(|_| ()).map_err(|e| compact_error(&e)),
                    }
                });
        }
        Effect::FetchPromptHistory { agent_id, cwd, session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "cwd": cwd.to_string_lossy(),
                    "filter_session_id": session_id,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/prompt_history",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize prompt_history params")
                            .into(),
                    );
                    match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let resp_value: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let prompts = resp_value
                                .get("result")
                                .and_then(|r| r.get("prompts"))
                                .or_else(|| resp_value.get("prompts"))
                                .and_then(|v| v.as_array())
                                .map(|arr| {
                                    arr
                                        .iter()
                                        .filter_map(|v| v.as_str().map(String::from))
                                        .collect::<Vec<String>>()
                                })
                                .unwrap_or_default();
                            TaskResult::PromptHistoryLoaded {
                                agent_id,
                                prompts,
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to fetch prompt history: {e}");
                            TaskResult::PromptHistoryLoaded {
                                agent_id,
                                prompts: Vec::new(),
                            }
                        }
                    }
                });
        }
        Effect::KillBgTask { session_id, task_id, source } => {
            let tx = acp_tx.clone();
            let sid = session_id.0.to_string();
            tasks
                .spawn(async move {
                    let params = xai_grok_shell::extensions::task::KillTaskRequest {
                        session_id: sid.clone(),
                        task_id: task_id.clone(),
                        source,
                    };
                    let req = acp::ExtRequest::new(
                        "x.ai/task/kill",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize kill params")
                            .into(),
                    );
                    match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let outcome = parse_kill_outcome(resp.0.get());
                            TaskResult::BgTaskKilled {
                                session_id: sid,
                                task_id,
                                outcome,
                            }
                        }
                        Err(e) => {
                            TaskResult::BgTaskKillFailed {
                                session_id: sid,
                                task_id,
                                error: sanitize_user_error(&e.to_string()),
                            }
                        }
                    }
                });
        }
        Effect::KillSubagent { session_id, subagent_id, attempt_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "subagentId": &subagent_id,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/subagent/cancel",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize cancel params")
                            .into(),
                    );
                    let outcome = match acp_send(req, &tx).await {
                        Ok(resp) => parse_subagent_kill_outcome(resp.0.get()),
                        Err(e) => {
                            tracing::warn!("Failed to cancel subagent: {e}");
                            SubagentKillOutcome::RpcFailed
                        }
                    };
                    TaskResult::KillSubagentComplete {
                        session_id,
                        subagent_id,
                        attempt_id,
                        outcome,
                    }
                });
        }
        Effect::DeleteScheduledTask { session_id, task_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "taskId": task_id,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/scheduler/delete",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize scheduler delete params")
                            .into(),
                    );
                    if let Err(e) = acp_send(req, &tx).await {
                        tracing::warn!(task_id, "Failed to delete scheduled task: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::DemoteToBackground { session_id, tool_call_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "terminalId": tool_call_id,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/terminal/background",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize background params")
                            .into(),
                    );
                    if let Err(e) = acp_send(req, &tx).await {
                        tracing::warn!("Failed to send background request: {e}");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::SetEffortAuto {
            agent_id,
            session_id,
            model_id,
        } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    use xai_grok_shell::sampling::types::REASONING_EFFORT_AUTO_META_KEY;
                    let mut meta = acp::Meta::new();
                    meta.insert(
                        REASONING_EFFORT_AUTO_META_KEY.to_string(),
                        serde_json::Value::Bool(true),
                    );
                    let req = acp::SetSessionModelRequest::new(session_id, model_id.clone())
                        .meta(Some(meta));
                    match acp_send(req, &tx).await {
                        // The optimistic flag was set at dispatch; a rejection
                        // means the harness never turned it on, so say so.
                        Ok(_) => TaskResult::EffortAutoSet {
                            agent_id,
                            model_id,
                            result: Ok(()),
                        },
                        Err(e) => TaskResult::EffortAutoSet {
                            agent_id,
                            model_id,
                            result: Err(sanitize_user_error(&e.to_string())),
                        },
                    }
                });
        }
        Effect::SwitchModel {
            agent_id,
            session_id,
            model_id,
            effort,
            prev_model_id,
        } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let meta = effort
                        .map(|eff| {
                            use xai_grok_shell::sampling::types::{
                                REASONING_EFFORT_META_KEY, reasoning_effort_meta_value,
                            };
                            let mut m = acp::Meta::new();
                            m.insert(
                                REASONING_EFFORT_META_KEY.to_string(),
                                reasoning_effort_meta_value(eff),
                            );
                            m
                        });
                    let req = acp::SetSessionModelRequest::new(
                            session_id,
                            model_id.clone(),
                        )
                        .meta(meta);
                    let result = acp_send(req, &tx)
                        .await
                        .map(|_| ())
                        .map_err(|e| {
                            use xai_grok_shell::agent::config::ModelSwitchIncompatibleAgentError;
                            if let Some(typed) = ModelSwitchIncompatibleAgentError::from_acp_error(
                                &e,
                            ) {
                                SwitchModelError::IncompatibleAgent {
                                    error: typed,
                                    prev_model_id: prev_model_id.clone(),
                                }
                            } else {
                                SwitchModelError::Other(sanitize_user_error(&e.to_string()))
                            }
                        });
                    TaskResult::SwitchModelComplete {
                        agent_id,
                        model_id,
                        effort,
                        result,
                        prev_model_id,
                    }
                });
        }
        Effect::ProbeClipboardAttachment { ctx, change_count } => {
            tasks
                .spawn(async move {
                    let probe_text = ctx.source.text().map(str::to_owned);
                    let probe_bracketed = ctx.source.is_bracketed();
                    let images_dir = match &ctx.target {
                        ClipboardPasteTarget::AgentPrompt { images_dir, .. } => {
                            images_dir.clone()
                        }
                        ClipboardPasteTarget::FeedbackModal { .. }
                        | ClipboardPasteTarget::DashboardDispatch
                        | ClipboardPasteTarget::DashboardPeek { .. } => None,
                    };
                    let (image, file_urls) = bounded_clipboard_probe(
                            std::time::Duration::from_secs(CLIPBOARD_PROBE_TIMEOUT_SECS),
                            move || probe_clipboard_attachment_blocking(
                                change_count,
                                probe_text,
                                probe_bracketed,
                                images_dir,
                            ),
                        )
                        .await;
                    TaskResult::ClipboardAttachmentProbed {
                        ctx,
                        image,
                        file_urls,
                    }
                });
        }
        Effect::RehydrateFeedbackImage { agent_id, modal_id, image_identity, path } => {
            tasks
                .spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                            xai_grok_feedback::read_regular_capped(
                                    &path,
                                    xai_grok_shell::session::MAX_FEEDBACK_IMAGE_BYTES,
                                )
                                .map_err(|error| error.to_string())
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result);
                    TaskResult::FeedbackImageRehydrated {
                        agent_id,
                        modal_id,
                        image_identity,
                        result,
                    }
                });
        }
        Effect::PreparePromptImagePreview { preparation } => {
            tasks
                .spawn(async move {
                    let preview = preparation.preview();
                    if tokio::task::spawn_blocking(move || preparation.run())
                        .await
                        .is_err()
                    {
                        preview.mark_failed();
                    }
                    TaskResult::PromptImagePreviewPrepared
                });
        }
        Effect::PlanDoctorFix { target, report, terminal, request } => {
            tasks
                .spawn(async move {
                    let result = tokio::task::spawn_blocking(move || match request {
                            crate::slash::command::DoctorRequest::ListFixes => {
                                Ok(
                                    actions::DoctorPlanningOutcome::Listing(
                                        crate::diagnostics::format_applicable_automatic_fixes(
                                            &report,
                                            &terminal,
                                        ),
                                    ),
                                )
                            }
                            crate::slash::command::DoctorRequest::Fix(id) => {
                                match crate::diagnostics::select_fix_plan(
                                    id,
                                    &report,
                                    &terminal,
                                ) {
                                    Ok(Some(plan)) => {
                                        Ok(actions::DoctorPlanningOutcome::Plan(Box::new(plan)))
                                    }
                                    Ok(None) => {
                                        Ok(
                                            actions::DoctorPlanningOutcome::RunLocally(
                                                crate::diagnostics::human_fix_command(id)
                                                    .unwrap_or_else(|| id.to_string()),
                                            ),
                                        )
                                    }
                                    Err(error) => Err(error.to_string()),
                                }
                            }
                            crate::slash::command::DoctorRequest::Report => {
                                unreachable!("report does not enter the planning effect")
                            }
                        })
                        .await
                        .map_err(|error| format!("Could not prepare the fix: {error}"))
                        .and_then(|result| result);
                    TaskResult::DoctorFixPlanned {
                        target,
                        result,
                    }
                });
        }
        Effect::ApplyDoctorFix { target, plan } => {
            tasks
                .spawn(async move {
                    let result = tokio::task::spawn_blocking(move || crate::diagnostics::apply_fix(
                            *plan,
                        ))
                        .await
                        .map_err(|error| format!("Could not apply the fix: {error}"))
                        .and_then(|result| result.map_err(|error| error.to_string()));
                    TaskResult::DoctorFixApplied {
                        target,
                        result,
                    }
                });
        }
        Effect::FetchChangelog => {
            tasks
                .spawn(async move {
                    let changelog = tokio::task::spawn_blocking(|| {
                            xai_grok_shell::util::changelog::ChangelogManager::new()
                                .fetch()
                        })
                        .await
                        .unwrap_or_else(|e| {
                            tracing::warn!(error = %e, "changelog fetch task failed");
                            xai_grok_shell::util::changelog::Changelog {
                                markdown: None,
                                entries: None,
                            }
                        });
                    TaskResult::ChangelogFetched {
                        markdown: changelog.markdown,
                        entries: changelog.entries.unwrap_or_default(),
                    }
                });
        }
        Effect::PersistAnnouncementsHidden { hidden_ids } => {
            tasks
                .spawn(async move {
                    xai_grok_announcements::write_hidden_announcement_ids(&hidden_ids)
                        .await;
                    TaskResult::AnnouncementsHiddenPersisted {
                        result: Ok(()),
                    }
                });
        }
        Effect::PersistPrivacyBannerAcked { acked_at } => {
            tasks
                .spawn(async move {
                    if let Err(e) = xai_grok_shell::util::config::set_privacy_banner_acked(
                            acked_at,
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "failed to persist privacy_banner_acked");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::PersistPluginCtaDismissed { plugin_id } => {
            tasks
                .spawn(async move {
                    if let Err(e) = xai_grok_shell::config::run_add_dismissed_plugin_cta(
                            plugin_id,
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "couldn't persist plugin CTA dismissal");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::PersistConsentAnswer { account, notice_id, version, acked } => {
            tasks
                .spawn(async move {
                    match xai_grok_shell::util::config::set_consent_answer(
                            account,
                            notice_id,
                            version,
                            acked,
                        )
                        .await
                    {
                        Ok(()) => TaskResult::CancelComplete,
                        Err(e) if !acked => {
                            TaskResult::ConsentPersistFailed {
                                error: e.to_string(),
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "consent ack not persisted");
                            TaskResult::CancelComplete
                        }
                    }
                });
        }
        Effect::RecordConsentUpstream { notice_id, version } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let request = acp::ExtRequest::new(
                        "x.ai/consent/record",
                        serde_json::value::to_raw_value(
                                &serde_json::json!({
                        "noticeId": notice_id,
                        "version": version,
                    }),
                            )
                            .expect("serialize params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(_) => {
                            TaskResult::ConsentRecorded {
                                notice_id,
                                version,
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, %notice_id, "consent record not filed");
                            TaskResult::CancelComplete
                        }
                    }
                });
        }
        Effect::PersistMemoryFullscreen { fullscreen } => {
            persist_hint(
                tasks,
                "memory_modal_fullscreen",
                fullscreen,
                "memory fullscreen",
            );
        }
        Effect::PersistDashboard(persisted) => {
            tasks
                .spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                            if let Err(e) = crate::views::dashboard::state::write_persisted(
                                &persisted,
                            ) {
                                tracing::warn!(error = %e, "failed to persist dashboard config");
                            }
                        })
                        .await;
                    if let Err(e) = result {
                        tracing::warn!(error = %e, "failed to persist dashboard: join error");
                    }
                    TaskResult::CancelComplete
                });
        }
        Effect::PersistWorktreeMode { mode, config_key } => {
            debug_assert!(
                config_key == "fork_worktree_mode" || config_key == "new_session_worktree_mode",
                "unexpected worktree config_key"
            );
            persist_hint(tasks, config_key, mode.as_config_str(), "worktree mode");
        }
        Effect::PersistPreferredModel { model_id, reasoning_effort } => {
            let model_id_str = model_id.0.to_string();
            tasks
                .spawn(async move {
                    let result = xai_grok_shell::util::config::persist_models_default(
                            Some(model_id_str),
                            reasoning_effort,
                        )
                        .await
                        .map_err(|e| e.to_string());
                    if let Err(ref e) = result {
                        tracing::warn!("failed to save default model preference: {e}");
                    }
                    TaskResult::PreferredModelPersisted {
                        result,
                    }
                });
        }
        Effect::PersistPermissionMode { canonical, session_id, persist } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(
                    persist_permission_mode_and_notify(
                        canonical,
                        session_id,
                        persist,
                        tx,
                    ),
                );
        }
        Effect::PersistSetting { key, value, rollback_value } => {
            tasks
                .spawn(async move {
                    match persist_setting(key, value.clone()).await {
                        Ok(()) => {
                            TaskResult::SettingPersisted {
                                key,
                                value,
                            }
                        }
                        Err(error) => {
                            TaskResult::SettingPersistFailed {
                                key,
                                rollback_value,
                                error,
                            }
                        }
                    }
                });
        }
        Effect::Authenticate {
            request_seq,
            method_id,
            use_oauth,
            force_interactive,
        } => {
            let tx = acp_tx.clone();
            let abort_handle = tasks
                .spawn(async move {
                    send_authenticate(
                            &tx,
                            request_seq,
                            method_id,
                            use_oauth,
                            force_interactive,
                        )
                        .await
                });
            meta.auth_abort_handle = Some((request_seq, abort_handle));
        }
        Effect::PollAuthUrl { request_seq } => {
            let tx = acp_tx.clone();
            let abort_handle = tasks
                .spawn(async move {
                    let mut auth_url: Option<String> = None;
                    let mut external = false;
                    let mut mode: Option<String> = None;
                    for i in 0..60 {
                        if i > 0 {
                            tokio::time::sleep(std::time::Duration::from_millis(50))
                                .await;
                        }
                        let params = serde_json::json!({});
                        let req = acp::ExtRequest::new(
                            "x.ai/auth/get_url",
                            serde_json::value::to_raw_value(&params)
                                .expect("serialize auth_url params")
                                .into(),
                        );
                        if let Ok(resp) = acp_send(req, &tx).await {
                            let v: serde_json::Value = serde_json::from_str(resp.0.get())
                                .unwrap_or_default();
                            external = v
                                .get("external_provider")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            mode = v
                                .get("mode")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                            auth_url = v
                                .get("auth_url")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());
                        }
                        if auth_url.is_some() {
                            break;
                        }
                    }
                    TaskResult::AuthUrlReady {
                        request_seq,
                        auth_url,
                        external,
                        mode,
                    }
                });
            meta.auth_url_poll_handle = Some((request_seq, abort_handle));
        }
        Effect::SubmitAuthCode { request_seq, code } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({ "code": code });
                    let req = acp::ExtRequest::new(
                        "x.ai/auth/submit_code",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize auth code params")
                            .into(),
                    );
                    match acp_send(req, &tx).await {
                        Ok(_) => {
                            TaskResult::AuthCodeSubmitted {
                                request_seq,
                            }
                        }
                        Err(e) => {
                            let error = e.to_string();
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
                });
        }
        Effect::FetchMcpsList { agent_id, session_id, cache } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "cache": cache,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/mcp/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize mcp/list params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                crate::views::mcps_modal::McpsListResponse,
                            >(inner.clone())
                                .map(crate::views::mcps_modal::convert_list_response)
                                .map_err(|_| "couldn't load server list".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "couldn't load server list: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::McpsListLoaded {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::McpAuthTrigger { agent_id, session_id, server_name } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "session_id": session_id.0.to_string(),
                    "server_name": server_name,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/mcp/auth_trigger",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize mcp/auth_trigger params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let result_obj = wrapper.get("result");
                            let status = result_obj
                                .and_then(|r| r.get("status"))
                                .and_then(|s| s.as_str())
                                .unwrap_or("unknown");
                            if status == "authenticated" {
                                Ok(
                                    crate::app::actions::McpAuthTriggerOutcome::Authenticated,
                                )
                            } else if status == "setup_required" {
                                let setup = result_obj
                                    .and_then(|r| r.get("setup"))
                                    .cloned()
                                    .and_then(|value| {
                                        serde_json::from_value::<
                                            crate::views::mcps_modal::McpSetupConfig,
                                        >(value)
                                            .ok()
                                    })
                                    .ok_or_else(|| "setup required".to_string());
                                setup
                                    .map(
                                        crate::app::actions::McpAuthTriggerOutcome::SetupRequired,
                                    )
                            } else {
                                let detail = result_obj
                                    .and_then(|r| r.get("error"))
                                    .and_then(|e| e.as_str())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| format!("auth status: {status}"));
                                Err(detail)
                            }
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(&format!("authentication failed: {e}")),
                            )
                        }
                    };
                    TaskResult::McpAuthTriggerDone {
                        agent_id,
                        server_name,
                        result,
                    }
                });
        }
        Effect::McpSetupSubmit { agent_id, session_id, server_name, values } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                    "serverName": server_name,
                    "values": values,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/mcp/setup",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize mcp/setup params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let result_obj = wrapper.get("result");
                            if result_obj
                                .and_then(|r| r.get("ok"))
                                .and_then(|ok| ok.as_bool())
                                .unwrap_or(false)
                            {
                                Ok(())
                            } else {
                                let detail = result_obj
                                    .and_then(|r| r.get("error"))
                                    .and_then(|e| e.as_str())
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| "setup failed".to_string());
                                Err(detail)
                            }
                        }
                        Err(e) => Err(sanitize_user_error(&format!("setup failed: {e}"))),
                    };
                    TaskResult::McpSetupSubmitDone {
                        agent_id,
                        server_name,
                        result,
                    }
                });
        }
        Effect::FetchHooksList { agent_id, session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/hooks/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize hooks/list params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                xai_hooks_plugins_types::HooksListResponse,
                            >(inner.clone())
                                .map_err(|_| "couldn't load hooks".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(&format!("couldn't load hooks: {e}")),
                            )
                        }
                    };
                    TaskResult::HooksListLoaded {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::FetchPluginsList { agent_id, session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/plugins/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize plugins/list params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                xai_hooks_plugins_types::PluginsListResponse,
                            >(inner.clone())
                                .map_err(|_| "couldn't load plugins".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(&format!("couldn't load plugins: {e}")),
                            )
                        }
                    };
                    TaskResult::PluginsListLoaded {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::FetchMemoryList { agent_id, session_id } => {
            use xai_grok_shell::extensions::memory::{
                MEMORY_LIST_METHOD, MemoryListRequest, MemoryListing,
            };
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let req_body = MemoryListRequest {
                        session_id: session_id.0.to_string(),
                    };
                    let req = acp::ExtRequest::new(
                        MEMORY_LIST_METHOD,
                        serde_json::value::to_raw_value(&req_body)
                            .expect("serialize memory/list params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            serde_json::from_str::<MemoryListing>(resp.0.get())
                                .map_err(|_| "Couldn't read the shell's reply.".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(&format!(
                        "Couldn't load memory: {e}"
                    )),
                            )
                        }
                    };
                    TaskResult::MemoryListLoaded {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::MemoryToggle { agent_id, session_id, enabled } => {
            use xai_grok_shell::extensions::memory::{
                MEMORY_TOGGLE_METHOD, MemoryToggleRequest, MemoryToggleResponse,
            };
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let req_body = MemoryToggleRequest {
                        session_id: session_id.0.to_string(),
                        enabled,
                    };
                    let req = acp::ExtRequest::new(
                        MEMORY_TOGGLE_METHOD,
                        serde_json::value::to_raw_value(&req_body)
                            .expect("serialize memory/toggle params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            serde_json::from_str::<MemoryToggleResponse>(resp.0.get())
                                .map_err(|_| "Couldn't read the shell's reply.".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "Couldn't change memory state: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::MemoryToggleResult {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::MemoryFlush { agent_id, session_id } => {
            use xai_grok_shell::extensions::memory::{
                MEMORY_FLUSH_METHOD, MemoryFlushResponse,
            };
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let result = memory_command_request::<
                        MemoryFlushResponse,
                    >(MEMORY_FLUSH_METHOD, &session_id, &tx)
                        .await;
                    TaskResult::MemoryFlushComplete {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::MemoryDream { agent_id, session_id } => {
            use xai_grok_shell::extensions::memory::{
                MEMORY_DREAM_METHOD, MemoryDreamResponse,
            };
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let result = memory_command_request::<
                        MemoryDreamResponse,
                    >(MEMORY_DREAM_METHOD, &session_id, &tx)
                        .await;
                    TaskResult::MemoryDreamComplete {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::MemoryForget { agent_id, session_id, path, expected_content_hash } => {
            use xai_grok_shell::extensions::memory::{
                MEMORY_FORGET_METHOD, MemoryForgetRequest, MemoryForgetResponse,
            };
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let req_body = MemoryForgetRequest {
                        session_id: session_id.0.to_string(),
                        path: path.clone(),
                        expected_content_hash,
                    };
                    let req = acp::ExtRequest::new(
                        MEMORY_FORGET_METHOD,
                        serde_json::value::to_raw_value(&req_body)
                            .expect("serialize memory/forget params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            serde_json::from_str::<MemoryForgetResponse>(resp.0.get())
                                .map_err(|_| "Couldn't read the shell's reply.".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "Couldn't delete the note: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::MemoryForgetResult {
                        agent_id,
                        path,
                        result,
                    }
                });
        }
        Effect::HooksAction { agent_id, session_id, action } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let req_body = xai_hooks_plugins_types::HooksActionRequest {
                        session_id: session_id.0.to_string(),
                        action,
                    };
                    let req = acp::ExtRequest::new(
                        "x.ai/hooks/action",
                        serde_json::value::to_raw_value(&req_body)
                            .expect("serialize hooks/action params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                xai_hooks_plugins_types::ActionOutcome,
                            >(inner.clone())
                                .map_err(|_| "couldn't complete hooks action".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "couldn't complete hooks action: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::HooksActionResult {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::PluginsAction { agent_id, session_id, action } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let req_body = xai_hooks_plugins_types::PluginsActionRequest {
                        session_id: session_id.0.to_string(),
                        action,
                    };
                    let req = acp::ExtRequest::new(
                        "x.ai/plugins/action",
                        serde_json::value::to_raw_value(&req_body)
                            .expect("serialize plugins/action params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                xai_hooks_plugins_types::ActionOutcome,
                            >(inner.clone())
                                .map_err(|_| "couldn't complete plugins action".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "couldn't complete plugins action: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::PluginsActionResult {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::FetchMarketplaceList { agent_id, session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/marketplace/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize marketplace/list params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                xai_hooks_plugins_types::MarketplaceListResponse,
                            >(inner.clone())
                                .map_err(|_| "couldn't load marketplace".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "couldn't load marketplace: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::MarketplaceListLoaded {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::FetchPluginCtaCatalog { agent_id, session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/marketplace/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize marketplace/list params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                xai_hooks_plugins_types::MarketplaceListResponse,
                            >(inner.clone())
                                .map_err(|_| "couldn't load marketplace".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "couldn't load marketplace: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::PluginCtaCatalogLoaded {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::FetchSkillsList { agent_id, session_id: _ } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "cwd": "."
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/skills/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize skills/list params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                Vec<
                                    xai_grok_tools::implementations::skills::types::SkillInfo,
                                >,
                            >(inner.get("skills").cloned().unwrap_or_default())
                                .map_err(|_| "couldn't load skills".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(&format!("couldn't load skills: {e}")),
                            )
                        }
                    };
                    TaskResult::SkillsListLoaded {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::FetchWorkflowsList { agent_id, session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/workflows/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize workflows/list params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                Vec<crate::views::extensions_modal::WorkflowInfo>,
                            >(inner.get("workflows").cloned().unwrap_or_default())
                                .map_err(|_| "couldn't load workflows".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "couldn't load workflows: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::WorkflowsListLoaded {
                        agent_id,
                        session_id,
                        result,
                    }
                });
        }
        Effect::ToggleSkill { agent_id, session_id: _, skill_name, enabled } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "name": skill_name,
                    "enabled": enabled,
                    "cwd": ".",
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/skills/toggle",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize skills/toggle params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            let parsed = serde_json::from_value::<
                                Vec<
                                    xai_grok_tools::implementations::skills::types::SkillInfo,
                                >,
                            >(inner.get("skills").cloned().unwrap_or_default())
                                .map_err(|_| "couldn't toggle skill".to_string());
                            if parsed.is_ok() {
                                let refresh = acp::ExtRequest::new(
                                    "x.ai/skills/refresh-baseline",
                                    serde_json::value::to_raw_value(&serde_json::json!({}))
                                        .expect("serialize empty params")
                                        .into(),
                                );
                                let _ = acp_send(refresh, &tx).await;
                            }
                            parsed
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(&format!("couldn't toggle skill: {e}")),
                            )
                        }
                    };
                    TaskResult::SkillsToggleDone {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::CheckMarketplaceUpdates { agent_id, session_id } => {
            if xai_grok_workspace::permission::resolution::managed_settings()
                .plugin_auto_update
                .is_disabled()
            {
                tracing::info!("session-start plugin auto-update disabled by managed policy");
                return (false, meta);
            }
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "sessionId": session_id.0.to_string(),
                });
                    let list_req = acp::ExtRequest::new(
                        "x.ai/marketplace/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize marketplace/list params")
                            .into(),
                    );
                    let outdated = match acp_send(list_req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                xai_hooks_plugins_types::MarketplaceListResponse,
                            >(inner.clone())
                                .ok()
                                .map(|r| {
                                    r
                                        .sources
                                        .iter()
                                        .flat_map(|s| {
                                            let source_url = s.source_url_or_path.clone();
                                            s.plugins
                                                .iter()
                                                .filter_map(move |p| {
                                                    if p.install_status == "update_available" {
                                                        Some((
                                                            p.name.clone(),
                                                            p.installed_version.clone().unwrap_or_else(|| "?".into()),
                                                            p.version.clone().unwrap_or_else(|| "?".into()),
                                                            source_url.clone(),
                                                            p.relative_path.clone(),
                                                        ))
                                                    } else {
                                                        None
                                                    }
                                                })
                                        })
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default()
                        }
                        Err(_) => Vec::new(),
                    };
                    if outdated.is_empty() {
                        return TaskResult::MarketplaceUpdatesAvailable {
                            agent_id,
                            updates: Vec::new(),
                        };
                    }
                    let mut succeeded = Vec::new();
                    for (name, old, new, source_url, rel_path) in outdated {
                        let action = xai_hooks_plugins_types::MarketplaceAction::Update {
                            source_url_or_path: source_url.clone(),
                            plugin_relative_path: rel_path.clone(),
                        };
                        let req_body = xai_hooks_plugins_types::MarketplaceActionRequest {
                            session_id: session_id.0.to_string(),
                            action,
                        };
                        let update_req = acp::ExtRequest::new(
                            "x.ai/marketplace/action",
                            serde_json::value::to_raw_value(&req_body)
                                .expect("serialize marketplace/action params")
                                .into(),
                        );
                        let update_succeeded = match acp_send(update_req, &tx).await {
                            Ok(resp) => {
                                let wrapper: serde_json::Value = serde_json::from_str(
                                        resp.0.get(),
                                    )
                                    .unwrap_or_default();
                                let inner = wrapper.get("result").unwrap_or(&wrapper);
                                serde_json::from_value::<
                                    xai_hooks_plugins_types::ActionOutcome,
                                >(inner.clone())
                                    .is_ok_and(|outcome| marketplace_outcome_succeeded(
                                        &outcome,
                                    ))
                            }
                            Err(_) => false,
                        };
                        if update_succeeded {
                            succeeded.push((name, old, new));
                        }
                    }
                    if !succeeded.is_empty() {
                        let notify_params = serde_json::json!({
                        "sessionId": session_id.0.to_string(),
                        "updates": succeeded,
                    });
                        let notify_req = acp::ExtRequest::new(
                            "x.ai/plugins/notify-updates",
                            serde_json::value::to_raw_value(&notify_params)
                                .expect("serialize notify-updates params")
                                .into(),
                        );
                        let _ = acp_send(notify_req, &tx).await;
                    }
                    TaskResult::MarketplaceUpdatesAvailable {
                        agent_id,
                        updates: succeeded,
                    }
                });
        }
        Effect::MarketplaceAction { agent_id, session_id, action } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let req_body = xai_hooks_plugins_types::MarketplaceActionRequest {
                        session_id: session_id.0.to_string(),
                        action,
                    };
                    let req = acp::ExtRequest::new(
                        "x.ai/marketplace/action",
                        serde_json::value::to_raw_value(&req_body)
                            .expect("serialize marketplace/action params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                xai_hooks_plugins_types::ActionOutcome,
                            >(inner.clone())
                                .map_err(|e| {
                                    tracing::debug!("failed to parse marketplace action response: {e}");
                                    "couldn't complete marketplace action".to_string()
                                })
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "couldn't complete marketplace action: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::MarketplaceActionResult {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::InstallPluginFromCta {
            agent_id,
            session_id,
            source_url_or_path,
            plugin_relative_path,
        } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let plugin_name = plugin_relative_path
                        .rsplit('/')
                        .next()
                        .unwrap_or(plugin_relative_path.as_str())
                        .to_string();
                    let action = xai_hooks_plugins_types::MarketplaceAction::Install {
                        source_url_or_path,
                        plugin_relative_path,
                    };
                    let req_body = xai_hooks_plugins_types::MarketplaceActionRequest {
                        session_id: session_id.0.to_string(),
                        action,
                    };
                    let req = acp::ExtRequest::new(
                        "x.ai/marketplace/action",
                        serde_json::value::to_raw_value(&req_body)
                            .expect("serialize marketplace/action params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                xai_hooks_plugins_types::ActionOutcome,
                            >(inner.clone())
                                .map_err(|e| {
                                    tracing::debug!("failed to parse marketplace action response: {e}");
                                    "couldn't complete marketplace action".to_string()
                                })
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "couldn't complete marketplace action: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::CtaPluginInstallDone {
                        agent_id,
                        plugin_name,
                        result,
                    }
                });
        }
        Effect::ReloadPluginsForCta { agent_id, session_id, plugin_name } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let req_body = xai_hooks_plugins_types::PluginsActionRequest {
                        session_id: session_id.0.to_string(),
                        action: xai_hooks_plugins_types::PluginsAction::Reload,
                    };
                    let req = acp::ExtRequest::new(
                        "x.ai/plugins/action",
                        serde_json::value::to_raw_value(&req_body)
                            .expect("serialize plugins/action params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                xai_hooks_plugins_types::ActionOutcome,
                            >(inner.clone())
                                .map_err(|_| "couldn't complete plugins action".to_string())
                        }
                        Err(e) => {
                            Err(
                                sanitize_user_error(
                                    &format!(
                        "couldn't complete plugins action: {e}"
                    ),
                                ),
                            )
                        }
                    };
                    TaskResult::CtaPluginReloadDone {
                        agent_id,
                        plugin_name,
                        result,
                    }
                });
        }
        Effect::FetchPluginCtaMcps { agent_id, session_id, plugin_name } => {
            let tx = acp_tx.clone();
            tasks.spawn(fetch_plugin_cta_mcps(agent_id, session_id, plugin_name, tx));
        }
        Effect::RetryPluginCtaMcps { agent_id, session_id, plugin_name } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    tokio::time::sleep(
                            std::time::Duration::from_millis(CTA_MCP_RETRY_DELAY_MS),
                        )
                        .await;
                    fetch_plugin_cta_mcps(agent_id, session_id, plugin_name, tx).await
                });
        }
        Effect::DismissCtaInstalled { agent_id, plugin_name } => {
            tasks
                .spawn(async move {
                    tokio::time::sleep(
                            std::time::Duration::from_millis(CTA_INSTALLED_DISMISS_MS),
                        )
                        .await;
                    TaskResult::CtaInstalledDismissTimeout {
                        agent_id,
                        plugin_name,
                    }
                });
        }
        Effect::UpsertMcpServer { agent_id, session_id, name, config } => {
            let tx = acp_tx.clone();
            let is_api_key_auth = session_flags.is_api_key_auth;
            tasks
                .spawn(async move {
                    #[derive(serde::Serialize)]
                    struct McpUpsertRequest {
                        session_id: String,
                        server_name: String,
                        #[serde(flatten)]
                        config: xai_grok_shell::util::config::McpServerConfig,
                    }
                    let req_body = McpUpsertRequest {
                        session_id: session_id.0.to_string(),
                        server_name: name,
                        config: *config,
                    };
                    let req = acp::ExtRequest::new(
                        "x.ai/mcp/upsert",
                        serde_json::value::to_raw_value(&req_body)
                            .expect("serialize mcp/upsert params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(_) => Ok(()),
                        Err(e) => Err(format_acp_error(&e, is_api_key_auth)),
                    };
                    TaskResult::McpToggleDone {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::DeleteMcpServer { agent_id, session_id, server_name } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    #[derive(serde::Serialize)]
                    struct McpDeleteRequest {
                        session_id: String,
                        server_name: String,
                    }
                    let req_body = McpDeleteRequest {
                        session_id: session_id.0.to_string(),
                        server_name,
                    };
                    let req = acp::ExtRequest::new(
                        "x.ai/mcp/delete",
                        serde_json::value::to_raw_value(&req_body)
                            .expect("serialize mcp/delete params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(_) => Ok(()),
                        Err(e) => {
                            Err(
                                sanitize_user_error(&format!("couldn't delete server: {e}")),
                            )
                        }
                    };
                    TaskResult::McpToggleDone {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::ToggleMcpServer { agent_id, session_id, server_name, enabled } => {
            let tx = acp_tx.clone();
            let is_api_key_auth = session_flags.is_api_key_auth;
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "session_id": session_id.0.to_string(),
                    "server_name": server_name,
                    "enabled": enabled,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/mcp/toggle",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize mcp/toggle params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(_) => Ok(()),
                        Err(e) => Err(format_acp_error(&e, is_api_key_auth)),
                    };
                    TaskResult::McpToggleDone {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::ToggleMcpTool {
            agent_id,
            session_id,
            server_name,
            tool_name,
            enabled,
        } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "session_id": session_id.0.to_string(),
                    "server_name": server_name,
                    "tool_name": tool_name,
                    "enabled": enabled,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/mcp/toggle_tool",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize mcp/toggle_tool params")
                            .into(),
                    );
                    let result = match acp_send(req, &tx).await {
                        Ok(_) => Ok(()),
                        Err(e) => {
                            Err(
                                sanitize_user_error(&format!("couldn't toggle tool: {e}")),
                            )
                        }
                    };
                    TaskResult::McpToggleDone {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::ShareSession { agent_id, session_id } => {
            use xai_grok_shell::session::{ShareSessionRequest, ShareSessionResponse};
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let request = acp::ExtRequest::new(
                        "x.ai/share_session",
                        serde_json::value::to_raw_value(
                                &ShareSessionRequest {
                                    session_id: session_id.0.to_string(),
                                },
                            )
                            .expect("serialize share session params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            if let Some(err) = wrapper.get("error") {
                                let msg = err
                                    .as_str()
                                    .map(String::from)
                                    .unwrap_or_else(|| "unknown error".to_string());
                                return TaskResult::ShareSessionFailed {
                                    agent_id,
                                    error: msg,
                                };
                            }
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            match serde_json::from_value::<
                                ShareSessionResponse,
                            >(inner.clone()) {
                                Ok(share_resp) => {
                                    TaskResult::ShareSessionComplete {
                                        agent_id,
                                        share_url: share_resp.share_url,
                                    }
                                }
                                Err(_) => {
                                    TaskResult::ShareSessionFailed {
                                        agent_id,
                                        error: "couldn't share session".to_string(),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            TaskResult::ShareSessionFailed {
                                agent_id,
                                error: sanitize_user_error(
                                    &format!("couldn't share session: {e}"),
                                ),
                            }
                        }
                    }
                });
        }
        Effect::FetchSessionAgentName { agent_id, session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    match fetch_session_info(&session_id, &tx).await {
                        Ok(info) => {
                            TaskResult::SessionAgentNameResolved {
                                agent_id,
                                agent_name: info.data.agent_name,
                            }
                        }
                        Err(e) => {
                            tracing::debug!("session agent name fetch failed: {e}");
                            TaskResult::SessionAgentNameResolved {
                                agent_id,
                                agent_name: None,
                            }
                        }
                    }
                });
        }
        Effect::ShowSessionInfo { agent_id, session_id, show_resolved_model, nonce } => {
            let is_api_key_auth = session_flags.is_api_key_auth;
            let api_key_env_set = xai_grok_shell::agent::auth_method::has_xai_api_key_env();
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    match fetch_session_info(&session_id, &tx).await {
                        Ok(info) => {
                            let title = lookup_session_title(&session_id, &info.cwd)
                                .await;
                            let text = format_session_info(
                                &info,
                                title.as_deref(),
                                show_resolved_model,
                                is_api_key_auth,
                                api_key_env_set,
                            );
                            let fields = session_info_fields(
                                &info,
                                title.as_deref(),
                                show_resolved_model,
                            );
                            TaskResult::SessionInfoComplete {
                                agent_id,
                                session_id,
                                info: Box::new(info),
                                text,
                                fields,
                                nonce,
                            }
                        }
                        Err(error) => {
                            TaskResult::SessionInfoFailed {
                                agent_id,
                                session_id,
                                error,
                                nonce,
                            }
                        }
                    }
                });
        }
        Effect::RenameSession { agent_id, session_id, title, cwd, kind } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    match session_rename_rpc(
                            &tx,
                            actions::RenameSessionRequest::for_rename(
                                session_id.0.to_string(),
                                title.clone(),
                                cwd.to_string_lossy().to_string(),
                                kind,
                            ),
                        )
                        .await
                    {
                        Ok(()) => {
                            TaskResult::RenameSessionComplete {
                                agent_id,
                                title,
                            }
                        }
                        Err(error) => {
                            TaskResult::RenameSessionFailed {
                                agent_id,
                                error,
                            }
                        }
                    }
                });
        }
        Effect::ResetSessionTitle {
            agent_id,
            session_id,
            cwd,
            kind,
            previous_display_name,
            previous_generated_title,
        } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    match session_rename_rpc(
                            &tx,
                            actions::RenameSessionRequest::for_reset(
                                session_id.0.to_string(),
                                cwd.to_string_lossy().to_string(),
                                kind,
                            ),
                        )
                        .await
                    {
                        Ok(()) => {
                            TaskResult::ResetSessionTitleComplete {
                                agent_id,
                            }
                        }
                        Err(error) => {
                            TaskResult::ResetSessionTitleFailed {
                                agent_id,
                                error,
                                previous_display_name,
                                previous_generated_title,
                            }
                        }
                    }
                });
        }
        Effect::DeleteSession { source, session_id, cwd, after } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    #[derive(serde::Serialize)]
                    #[serde(rename_all = "camelCase")]
                    struct DeleteRequest {
                        session_id: String,
                        cwd: String,
                    }
                    let request = acp::ExtRequest::new(
                        "x.ai/session/delete",
                        serde_json::value::to_raw_value(
                                &DeleteRequest {
                                    session_id: session_id.clone(),
                                    cwd,
                                },
                            )
                            .expect("serialize delete params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            if let Some(err) = wrapper
                                .get("error")
                                .filter(|v| !v.is_null())
                            {
                                let msg = err
                                    .as_str()
                                    .map(String::from)
                                    .unwrap_or_else(|| err.to_string());
                                return TaskResult::DeleteSessionFailed {
                                    source,
                                    session_id,
                                    error: msg,
                                };
                            }
                            TaskResult::DeleteSessionComplete {
                                source,
                                session_id,
                                after,
                            }
                        }
                        Err(e) => {
                            TaskResult::DeleteSessionFailed {
                                source,
                                session_id,
                                error: sanitize_user_error(
                                    &format!("couldn't delete session: {e}"),
                                ),
                            }
                        }
                    }
                });
        }
        Effect::SetCodingDataSharing { agent_id, opted_in, seq } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let request = acp::ExtRequest::new(
                        "x.ai/privacy/setCodingDataRetention",
                        serde_json::value::to_raw_value(
                                &serde_json::json!({ "codingDataRetentionOptOut": !opted_in }),
                            )
                            .expect("serialize params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = match serde_json::from_str(
                                resp.0.get(),
                            ) {
                                Ok(v) => v,
                                Err(e) => {
                                    return TaskResult::CodingDataSharingFailed {
                                        agent_id,
                                        error: format!("malformed response: {e}"),
                                        seq,
                                    };
                                }
                            };
                            if let Some(err) = wrapper
                                .get("error")
                                .filter(|v| !v.is_null())
                            {
                                let msg = err
                                    .as_str()
                                    .map(String::from)
                                    .unwrap_or_else(|| err.to_string());
                                return TaskResult::CodingDataSharingFailed {
                                    agent_id,
                                    error: msg,
                                    seq,
                                };
                            }
                            let confirmed_opted_in = wrapper
                                .get("codingDataRetentionOptOut")
                                .and_then(|v| v.as_bool())
                                .map(|opt_out| !opt_out)
                                .unwrap_or(opted_in);
                            TaskResult::CodingDataSharingUpdated {
                                agent_id,
                                opted_in: confirmed_opted_in,
                                seq,
                            }
                        }
                        Err(e) => {
                            TaskResult::CodingDataSharingFailed {
                                agent_id,
                                error: format!("{e}"),
                                seq,
                            }
                        }
                    }
                });
        }
        Effect::ShowContextInfo { agent_id, session_id, nonce } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    match fetch_session_info(&session_id, &tx).await {
                        Ok(info) => {
                            TaskResult::ContextInfoComplete {
                                agent_id,
                                session_id,
                                info: Box::new(info),
                                nonce,
                            }
                        }
                        Err(error) => {
                            TaskResult::ContextInfoFailed {
                                agent_id,
                                session_id,
                                error,
                                nonce,
                            }
                        }
                    }
                });
        }
        Effect::FetchSessionUsage { agent_id, session_id, nonce } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    match fetch_session_usage(&session_id, &tx).await {
                        Ok(usage) => {
                            TaskResult::SessionUsageComplete {
                                agent_id,
                                session_id,
                                usage: Box::new(usage),
                                nonce,
                            }
                        }
                        Err(error) => {
                            TaskResult::SessionUsageFailed {
                                agent_id,
                                session_id,
                                error,
                                nonce,
                            }
                        }
                    }
                });
        }
        Effect::SendFeedback {
            agent_id,
            session_id,
            feedback_text,
            images,
            metadata,
            request_trace_upload_token,
            draft,
            origin,
        } => {
            use xai_grok_shell::session::ClientType;
            use xai_grok_shell::session::acp_types::{
                ClientFeedbackInput, FeedbackDraftEditedBody, FeedbackDraftSendRequest,
            };
            let terminal_info = Some(
                crate::terminal::terminal_context().feedback_info(),
            );
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let image_count = images.len();
                    let raw_params = if let Some(draft) = draft {
                        serde_json::value::to_raw_value(
                            &FeedbackDraftSendRequest {
                                session_id: session_id.0.to_string(),
                                draft_id: draft.draft_id,
                                request_trace_upload_token,
                                edited_body: FeedbackDraftEditedBody {
                                    input: xai_grok_feedback::FeedbackDraftInput {
                                        title: draft.title,
                                        details: draft.details,
                                        area: draft.area,
                                        r#type: draft.r#type,
                                        task_category: draft.task_category,
                                        failure_mode: draft.failure_mode,
                                    },
                                    images: draft.images,
                                    client_version: Some(xai_grok_version::VERSION.to_string()),
                                    terminal_info,
                                },
                            },
                        )
                    } else {
                        serde_json::value::to_raw_value(
                                &ClientFeedbackInput {
                                    session_id: session_id.0.to_string(),
                                    client_type: ClientType::Tui,
                                    rating_type: None,
                                    rating_value: None,
                                    feedback_text: Some(feedback_text.clone()),
                                    images,
                                    feedback_categories: vec![],
                                    context_type: None,
                                    turn_number: None,
                                    request_id: None,
                                    client_version: Some(xai_grok_version::VERSION.to_string()),
                                    metadata,
                                    terminal_info,
                                    request_trace_upload_token,
                                },
                            )
                    };
                    let raw_params = match raw_params {
                        Ok(v) => v,
                        Err(e) => {
                            return TaskResult::FeedbackFailed {
                                agent_id,
                                origin,
                                feedback_text,
                                image_count,
                                error: sanitize_user_error(
                                    &format!(
                                "couldn't serialize feedback: {e}"
                            ),
                                ),
                            };
                        }
                    };
                    let request = acp::ExtRequest::new(
                        "x.ai/feedback",
                        raw_params.into(),
                    );
                    const FEEDBACK_SEND_ACP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(
                        30,
                    );
                    match tokio::time::timeout(
                            FEEDBACK_SEND_ACP_TIMEOUT,
                            acp_send(request, &tx),
                        )
                        .await
                    {
                        Err(_elapsed) => {
                            TaskResult::FeedbackComplete {
                                agent_id,
                                origin,
                                outcome: xai_grok_shell::session::FeedbackOutcome::OutcomeUnknown,
                                trace_upload_token: None,
                            }
                        }
                        Ok(send) => {
                            match send {
                                Ok(response) => {
                                    match serde_json::from_str::<
                                        xai_grok_shell::session::FeedbackResponse,
                                    >(response.0.get()) {
                                        Ok(response) => {
                                            TaskResult::FeedbackComplete {
                                                agent_id,
                                                origin,
                                                outcome: response
                                                    .outcome
                                                    .unwrap_or(
                                                        xai_grok_shell::session::FeedbackOutcome::Submitted,
                                                    ),
                                                trace_upload_token: response.trace_upload_token,
                                            }
                                        }
                                        Err(error) => {
                                            TaskResult::FeedbackFailed {
                                                agent_id,
                                                origin,
                                                feedback_text,
                                                image_count,
                                                error: sanitize_user_error(
                                                    &format!(
                                "couldn't decode feedback response: {error}"
                            ),
                                                ),
                                            }
                                        }
                                    }
                                }
                                Err(
                                    error,
                                ) if matches!(
                            xai_acp_lib::acp_channel_failure(&error),
                            Some(xai_acp_lib::AcpChannelFailure::RecvFailed)
                        ) => {
                                    TaskResult::FeedbackComplete {
                                        agent_id,
                                        origin,
                                        outcome: xai_grok_shell::session::FeedbackOutcome::OutcomeUnknown,
                                        trace_upload_token: None,
                                    }
                                }
                                Err(e) => {
                                    TaskResult::FeedbackFailed {
                                        agent_id,
                                        origin,
                                        feedback_text,
                                        image_count,
                                        error: sanitize_user_error(
                                            &format!("couldn't send feedback: {e}"),
                                        ),
                                    }
                                }
                            }
                        }
                    }
                });
        }
        Effect::FeedbackDraftRequest { agent_id, session_id, request } => {
            let tx = acp_tx.clone();
            const FEEDBACK_DRAFT_ACP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(
                15,
            );
            tasks
                .spawn(async move {
                    match request {
                        crate::views::feedback_modal::FeedbackDraftRequest::List {
                            modal_id,
                            generation,
                        } => {
                            let raw_params = match serde_json::value::to_raw_value(
                                &serde_json::json!({
                            "session_id": session_id.0.to_string(),
                        }),
                            ) {
                                Ok(value) => value,
                                Err(error) => {
                                    return TaskResult::FeedbackDraftListComplete {
                                        agent_id,
                                        modal_id,
                                        generation,
                                        result: Err(
                                            sanitize_user_error(
                                                &format!(
                                        "couldn't serialize feedback draft list: {error}"
                                    ),
                                            ),
                                        ),
                                    };
                                }
                            };
                            let request = acp::ExtRequest::new(
                                "x.ai/feedback/drafts/list",
                                raw_params.into(),
                            );
                            let result = match tokio::time::timeout(
                                    FEEDBACK_DRAFT_ACP_TIMEOUT,
                                    acp_send(request, &tx),
                                )
                                .await
                            {
                                Ok(send) => {
                                    send.map_err(|error| sanitize_user_error(
                                            &error.to_string(),
                                        ))
                                        .and_then(|response| {
                                            #[derive(serde::Deserialize)]
                                            struct DraftListResponse {
                                                drafts: Vec<xai_grok_feedback::FeedbackDraft>,
                                            }
                                            serde_json::from_str::<DraftListResponse>(response.0.get())
                                                .map(|response| response.drafts)
                                                .map_err(|error| sanitize_user_error(&error.to_string()))
                                        })
                                }
                                Err(_elapsed) => {
                                    Err("feedback draft list timed out".to_string())
                                }
                            };
                            TaskResult::FeedbackDraftListComplete {
                                agent_id,
                                modal_id,
                                generation,
                                result,
                            }
                        }
                        crate::views::feedback_modal::FeedbackDraftRequest::Load(
                            load,
                        ) => {
                            let raw_params = match serde_json::value::to_raw_value(
                                &serde_json::json!({
                            "session_id": session_id.0.to_string(),
                            "draft_id": load.draft_id,
                        }),
                            ) {
                                Ok(value) => value,
                                Err(error) => {
                                    return TaskResult::FeedbackDraftLoadComplete {
                                        agent_id,
                                        load,
                                        result: Err(
                                            sanitize_user_error(
                                                &format!(
                                        "couldn't serialize feedback draft get: {error}"
                                    ),
                                            ),
                                        ),
                                    };
                                }
                            };
                            let request = acp::ExtRequest::new(
                                "x.ai/feedback/drafts/get",
                                raw_params.into(),
                            );
                            let result = match tokio::time::timeout(
                                    FEEDBACK_DRAFT_ACP_TIMEOUT,
                                    acp_send(request, &tx),
                                )
                                .await
                            {
                                Ok(send) => {
                                    send.map_err(|error| sanitize_user_error(
                                            &error.to_string(),
                                        ))
                                        .and_then(|response| {
                                            #[derive(serde::Deserialize)]
                                            struct DraftGetResponse {
                                                draft: xai_grok_feedback::FeedbackDraft,
                                            }
                                            serde_json::from_str::<DraftGetResponse>(response.0.get())
                                                .map(|response| response.draft)
                                                .map_err(|error| sanitize_user_error(&error.to_string()))
                                        })
                                }
                                Err(_elapsed) => {
                                    Err("feedback draft load timed out".to_string())
                                }
                            };
                            TaskResult::FeedbackDraftLoadComplete {
                                agent_id,
                                load,
                                result,
                            }
                        }
                        crate::views::feedback_modal::FeedbackDraftRequest::Delete(
                            delete,
                        ) => {
                            let raw_params = match serde_json::value::to_raw_value(
                                &serde_json::json!({
                            "session_id": session_id.0.to_string(),
                            "draft_id": delete.draft_id,
                        }),
                            ) {
                                Ok(value) => value,
                                Err(error) => {
                                    return TaskResult::FeedbackDraftDeleteComplete {
                                        agent_id,
                                        delete,
                                        result: Err(
                                            sanitize_user_error(
                                                &format!(
                                        "couldn't serialize feedback draft delete: {error}"
                                    ),
                                            ),
                                        ),
                                    };
                                }
                            };
                            let request = acp::ExtRequest::new(
                                "x.ai/feedback/drafts/delete",
                                raw_params.into(),
                            );
                            let result = match tokio::time::timeout(
                                    FEEDBACK_DRAFT_ACP_TIMEOUT,
                                    acp_send(request, &tx),
                                )
                                .await
                            {
                                Ok(send) => {
                                    send.map(|_| ())
                                        .map_err(|error| sanitize_user_error(&error.to_string()))
                                }
                                Err(_elapsed) => {
                                    Err("feedback draft delete timed out".to_string())
                                }
                            };
                            TaskResult::FeedbackDraftDeleteComplete {
                                agent_id,
                                delete,
                                result,
                            }
                        }
                        crate::views::feedback_modal::FeedbackDraftRequest::Update(
                            update,
                        ) => {
                            let raw_params = match serde_json::value::to_raw_value(
                                &xai_grok_shell::session::FeedbackDraftUpdateRequest {
                                    session_id: session_id.0.to_string(),
                                    draft_id: update.draft_id.clone(),
                                    input: xai_grok_feedback::FeedbackDraftInput {
                                        title: update.title.clone(),
                                        details: update.details.clone(),
                                        area: update.area.clone(),
                                        r#type: update.r#type,
                                        task_category: update.task_category,
                                        failure_mode: update.failure_mode,
                                    },
                                },
                            ) {
                                Ok(value) => value,
                                Err(error) => {
                                    return TaskResult::FeedbackDraftUpdateComplete {
                                        agent_id,
                                        update,
                                        result: Err(
                                            sanitize_user_error(
                                                &format!(
                                        "couldn't serialize feedback draft update: {error}"
                                    ),
                                            ),
                                        ),
                                    };
                                }
                            };
                            let request = acp::ExtRequest::new(
                                "x.ai/feedback/drafts/update",
                                raw_params.into(),
                            );
                            let result = match tokio::time::timeout(
                                    FEEDBACK_DRAFT_ACP_TIMEOUT,
                                    acp_send(request, &tx),
                                )
                                .await
                            {
                                Ok(send) => {
                                    send.map(|_| ())
                                        .map_err(|error| sanitize_user_error(&error.to_string()))
                                }
                                Err(_elapsed) => {
                                    Err("feedback draft update timed out".to_string())
                                }
                            };
                            TaskResult::FeedbackDraftUpdateComplete {
                                agent_id,
                                update,
                                result,
                            }
                        }
                    }
                });
        }
        Effect::UploadFeedbackTrace {
            agent_id,
            session_id,
            submission_id,
            intent,
            trace_upload_token,
        } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let raw_params = match serde_json::value::to_raw_value(
                        &UploadTraceRequest {
                            session_id: session_id.0.to_string(),
                            intent,
                            trace_upload_token,
                        },
                    ) {
                        Ok(v) => v,
                        Err(e) => {
                            return TaskResult::FeedbackTraceUploaded {
                                agent_id,
                                submission_id,
                                error: Some(
                                    sanitize_user_error(
                                        &format!(
                                "couldn't serialize trace upload: {e}"
                            ),
                                    ),
                                ),
                            };
                        }
                    };
                    let request = acp::ExtRequest::new(
                        "x.ai/feedback/upload-trace",
                        raw_params.into(),
                    );
                    match tokio::time::timeout(
                            std::time::Duration::from_millis(
                                crate::app::dispatch::FEEDBACK_TRACE_UPLOAD_TIMEOUT_MS,
                            ),
                            acp_send(request, &tx),
                        )
                        .await
                    {
                        Ok(Ok(_)) => {
                            TaskResult::FeedbackTraceUploaded {
                                agent_id,
                                submission_id,
                                error: None,
                            }
                        }
                        Ok(Err(e)) => {
                            TaskResult::FeedbackTraceUploaded {
                                agent_id,
                                submission_id,
                                error: Some(sanitize_user_error(&format!("{e}"))),
                            }
                        }
                        Err(_) => {
                            TaskResult::FeedbackTraceUploaded {
                                agent_id,
                                submission_id,
                                error: Some("trace upload timed out".to_string()),
                            }
                        }
                    }
                });
        }
        Effect::RewriteMemoryNote {
            agent_id,
            session_id,
            raw_text,
            context_summary,
            nonce,
        } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let request = acp::ExtRequest::new(
                        "x.ai/memory/rewrite",
                        serde_json::value::to_raw_value(
                                &serde_json::json!({
                        "sessionId": session_id.0.to_string(),
                        "rawText": raw_text,
                        "contextSummary": context_summary,
                    }),
                            )
                            .expect("serialize memory/rewrite params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let parsed: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let text = parsed
                                .get("result")
                                .and_then(|r| r.get("rewritten"))
                                .or_else(|| parsed.get("rewritten"))
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .unwrap_or(raw_text);
                            TaskResult::MemoryNoteRewritten {
                                agent_id,
                                result: Ok(text),
                                nonce,
                            }
                        }
                        Err(_) => {
                            TaskResult::MemoryNoteRewritten {
                                agent_id,
                                result: Ok(raw_text),
                                nonce,
                            }
                        }
                    }
                });
        }
        Effect::SaveMemoryNote { agent_id, text, cwd, pinned_mode } => {
            tasks
                .spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                            let mode = match pinned_mode {
                                Some(mode) => mode,
                                None => xai_grok_shell::config::load_memory_mode()?,
                            };
                            let storage = xai_grok_shell::session::memory::MemoryStorage::new_for_mode(
                                &cwd,
                                None,
                                mode,
                            );
                            storage.save_remember_note(&text)
                        })
                        .await
                        .map_err(|e| format!("task join error: {e}"))
                        .and_then(|r| r.map_err(|e| format!("{e}")));
                    TaskResult::MemoryNoteSaved {
                        agent_id,
                        result,
                    }
                });
        }
        Effect::SendBtw {
            agent_id,
            session_id,
            question,
            images,
            cwd,
            minimal_request_id,
        } => {
            let tx = acp_tx.clone();
            let is_api_key_auth = session_flags.is_api_key_auth;
            tasks
                .spawn(async move {
                    let attached = images.len();
                    let question_fallback = question.clone();
                    let session_fallback = session_id.clone();
                    let images = BtwSendImages(images);
                    let prepared = tokio::task::spawn_blocking(move || {
                            let mut encoded = crate::app::dispatch::notes::encode_btw_images(
                                &question,
                                &images.0,
                                &cwd,
                            );
                            let image_notice = btw_image_notice(
                                encoded.omitted,
                                attached,
                            );
                            let mut question = question;
                            if let Some(notice) = image_notice.as_deref() {
                                append_btw_notice(
                                    &mut question,
                                    encoded.blocks.as_mut(),
                                    notice,
                                );
                            }
                            let params = build_btw_params(
                                &session_id,
                                &question,
                                encoded.blocks.as_deref(),
                            );
                            let raw = serde_json::value::to_raw_value(&params)
                                .expect("serialize btw params");
                            (raw, image_notice)
                        })
                        .await
                        .unwrap_or_else(|e| {
                            tracing::warn!("btw image encode task failed: {e}");
                            let notice = btw_image_notice(attached, attached);
                            let mut question = question_fallback;
                            if let Some(notice) = notice.as_deref() {
                                append_btw_notice(&mut question, None, notice);
                            }
                            let params = build_btw_params(
                                &session_fallback,
                                &question,
                                None,
                            );
                            let raw = serde_json::value::to_raw_value(&params)
                                .expect("serialize btw params");
                            (raw, notice)
                        });
                    let (raw, image_notice) = prepared;
                    let request = acp::ExtRequest::new("x.ai/btw", raw.into());
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let parsed: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let answer = parsed
                                .get("result")
                                .and_then(|r| r.get("answer"))
                                .and_then(|a| a.as_str())
                                .unwrap_or("No response")
                                .to_string();
                            TaskResult::BtwResponse {
                                agent_id,
                                result: Ok(answer),
                                minimal_request_id,
                                image_notice,
                            }
                        }
                        Err(e) => {
                            TaskResult::BtwResponse {
                                agent_id,
                                result: Err(format_acp_error(&e, is_api_key_auth)),
                                minimal_request_id,
                                image_notice,
                            }
                        }
                    }
                });
        }
        Effect::SendRecap { session_id, auto } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let request = acp::ExtRequest::new(
                        "x.ai/recap",
                        serde_json::value::to_raw_value(
                                &serde_json::json!({
                        "sessionId": session_id.0.to_string(),
                        "auto": auto,
                    }),
                            )
                            .expect("serialize recap params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(_) => {
                            TaskResult::RecapRequested {
                                session_id,
                                auto,
                                error: None,
                            }
                        }
                        Err(e) => {
                            TaskResult::RecapRequested {
                                session_id,
                                auto,
                                error: Some(format!("recap request failed: {e}")),
                            }
                        }
                    }
                });
        }
        Effect::SendInterject {
            agent_id,
            session_id,
            text,
            interjection_id,
            blocks,
        } => {
            spawn_ordered_interjects(
                tasks,
                acp_tx,
                agent_id,
                session_id,
                vec![(text, interjection_id, blocks)],
            );
        }
        Effect::FetchCatalogEntry { kind, name } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({ "kind": kind, "name": name });
                    let request = acp::ExtRequest::new(
                        "x.ai/bundle/entry/get",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize bundle/entry/get params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            if let Some(err) = wrapper.get("error") {
                                let msg = err
                                    .as_str()
                                    .map(String::from)
                                    .unwrap_or_else(|| "unknown error".to_string());
                                return TaskResult::CatalogEntryFailed {
                                    error: msg,
                                };
                            }
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            match serde_json::from_value::<
                                super::bundle::EntryGetResult,
                            >(inner.clone()) {
                                Ok(r) => {
                                    TaskResult::CatalogEntryReady {
                                        kind: r.kind,
                                        name: r.name,
                                        content: r.content,
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!("failed to parse catalog entry response: {e}");
                                    TaskResult::CatalogEntryFailed {
                                        error: "couldn't load entry".to_string(),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            TaskResult::CatalogEntryFailed {
                                error: sanitize_user_error(
                                    &format!("couldn't load entry: {e}"),
                                ),
                            }
                        }
                    }
                });
        }
        Effect::FetchBundleStatus => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let request = acp::ExtRequest::new(
                        "x.ai/bundle/status",
                        serde_json::value::to_raw_value(&serde_json::json!({}))
                            .expect("serialize bundle/status params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            if let Some(err) = wrapper.get("error") {
                                let msg = err
                                    .as_str()
                                    .map(String::from)
                                    .unwrap_or_else(|| "unknown error".to_string());
                                return TaskResult::BundleStatusFailed {
                                    error: msg,
                                };
                            }
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            match serde_json::from_value::<
                                super::bundle::BundleStatusResult,
                            >(inner.clone()) {
                                Ok(r) => {
                                    TaskResult::BundleStatusReady {
                                        has_cache: r.has_cache,
                                        version: r.version,
                                        personas: r.personas,
                                        roles: r.roles,
                                        agents: r.agents,
                                        skills: r.skills,
                                        persona_details: r.persona_details,
                                        role_details: r.role_details,
                                    }
                                }
                                Err(_) => {
                                    TaskResult::BundleStatusFailed {
                                        error: "couldn't fetch bundle status".to_string(),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            TaskResult::BundleStatusFailed {
                                error: sanitize_user_error(
                                    &format!("couldn't fetch bundle status: {e}"),
                                ),
                            }
                        }
                    }
                });
        }
        Effect::RefreshAvailableCommands { agent_id, session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({ "sessionId": session_id });
                    let req = acp::ExtRequest::new(
                        "x.ai/commands/list",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize commands/list params")
                            .into(),
                    );
                    match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let inner = wrapper.get("result").unwrap_or(&wrapper);
                            let commands: Vec<acp::AvailableCommand> = inner
                                .get("commands")
                                .and_then(|v| serde_json::from_value(v.clone()).ok())
                                .unwrap_or_default();
                            TaskResult::AvailableCommandsRefreshed {
                                agent_id,
                                commands,
                            }
                        }
                        Err(e) => {
                            tracing::warn!("commands/list refresh failed: {e}");
                            TaskResult::AvailableCommandsRefreshed {
                                agent_id,
                                commands: vec![],
                            }
                        }
                    }
                });
        }
        Effect::FetchRewindPoints { agent_id, session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let request = acp::ExtRequest::new(
                        "x.ai/rewind/points",
                        serde_json::value::to_raw_value(
                                &serde_json::json!({
                        "sessionId": session_id.0.to_string()
                    }),
                            )
                            .expect("serialize rewind/points params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            if let Some(err) = wrapper
                                .get("error")
                                .filter(|v| !v.is_null())
                            {
                                return TaskResult::RewindPointsFailed {
                                    agent_id,
                                    error: err.as_str().unwrap_or("unknown error").to_string(),
                                };
                            }
                            let result_val = wrapper
                                .get("result")
                                .cloned()
                                .unwrap_or(wrapper.clone());
                            match serde_json::from_value::<
                                crate::views::rewind::RewindPointsResponse,
                            >(result_val) {
                                Ok(r) => {
                                    TaskResult::RewindPointsLoaded {
                                        agent_id,
                                        points: r.rewind_points,
                                    }
                                }
                                Err(e) => {
                                    TaskResult::RewindPointsFailed {
                                        agent_id,
                                        error: format!("invalid response: {e}"),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            TaskResult::RewindPointsFailed {
                                agent_id,
                                error: sanitize_user_error(&e.to_string()),
                            }
                        }
                    }
                });
        }
        Effect::RewindExecute { agent_id, session_id, target_prompt_index } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let request = acp::ExtRequest::new(
                        "x.ai/rewind/execute",
                        serde_json::value::to_raw_value(
                                &rewind_execute_params(
                                    session_id.0.as_ref(),
                                    target_prompt_index,
                                ),
                            )
                            .expect("serialize rewind/execute params")
                            .into(),
                    );
                    match acp_send(request, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let result_val = wrapper
                                .get("result")
                                .cloned()
                                .unwrap_or(wrapper.clone());
                            match serde_json::from_value::<
                                crate::views::rewind::RewindResponse,
                            >(result_val) {
                                Ok(r) => {
                                    TaskResult::RewindExecuteComplete {
                                        agent_id,
                                        response: r,
                                    }
                                }
                                Err(e) => {
                                    TaskResult::RewindExecuteFailed {
                                        agent_id,
                                        error: format!("invalid response: {e}"),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            TaskResult::RewindExecuteFailed {
                                agent_id,
                                error: sanitize_user_error(&e.to_string()),
                            }
                        }
                    }
                });
        }
        Effect::DeepSearchSessions { host, generation, query, seq, headless_policy } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let deadline = tokio::time::Instant::now()
                        + std::time::Duration::from_secs(30);
                    let retry_interval = std::time::Duration::from_secs(3);
                    let mut results = Vec::new();
                    loop {
                        let params = serde_json::json!({
                        "query": query,
                        "limit": 20,
                        "includeContent": true,
                        "headless": headless_policy.as_wire_str(),
                    });
                        let request = acp::ExtRequest::new(
                            "x.ai/session/search",
                            serde_json::value::to_raw_value(&params)
                                .expect("serialize deep search params")
                                .into(),
                        );
                        let remaining = deadline
                            .saturating_duration_since(tokio::time::Instant::now());
                        if remaining.is_zero() {
                            break;
                        }
                        let result = tokio::time::timeout(
                                remaining,
                                acp_send(request, &tx),
                            )
                            .await;
                        match result {
                            Ok(Ok(resp)) => {
                                let wrapper: serde_json::Value = serde_json::from_str(
                                        resp.0.get(),
                                    )
                                    .unwrap_or_default();
                                let payload = wrapper.get("result").unwrap_or(&wrapper);
                                if let Some(hits) = payload.get("results") {
                                    results = serde_json::from_value::<
                                        Vec<
                                            xai_grok_shell::extensions::session_search::SearchSessionHit,
                                        >,
                                    >(hits.clone())
                                        .unwrap_or_default();
                                }
                                let bootstrapping = payload
                                    .get("bootstrapping")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false);
                                if !bootstrapping {
                                    break;
                                }
                            }
                            Ok(Err(e)) => {
                                tracing::warn!("deep search failed: {e}");
                                break;
                            }
                            Err(_) => {
                                tracing::warn!("deep search timed out");
                                break;
                            }
                        }
                        tokio::time::sleep(retry_interval).await;
                    }
                    TaskResult::DeepSearchResults {
                        host,
                        generation,
                        results,
                        seq,
                    }
                });
        }
        Effect::ForkSession {
            agent_id,
            parent_session_id,
            parent_cwd,
            parent_is_worktree,
            new_session_id,
        } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let sid_str = parent_session_id.0.to_string();
                    let parent_cwd_str = parent_cwd.to_string_lossy().into_owned();
                    if let Some(ref nid) = new_session_id
                        && let Err(e) = crate::app::session_startup::ensure_session_id_available(
                            nid,
                            &parent_cwd_str,
                        )
                    {
                        return TaskResult::ForkSessionFailed {
                            agent_id,
                            error: sanitize_user_error(&e.to_string()),
                        };
                    }
                    let payload = crate::app::session_startup::fork_session_params(
                        &sid_str,
                        &parent_cwd,
                        new_session_id.as_deref(),
                        parent_is_worktree,
                    );
                    let req = acp::ExtRequest::new(
                        "x.ai/session/fork",
                        serde_json::value::to_raw_value(&payload)
                            .expect("serialize fork params")
                            .into(),
                    );
                    match acp_send(req, &tx).await {
                        Ok(resp) => {
                            if let Some(err) = crate::app::session_startup::fork_response_error(
                                resp.0.get(),
                            ) {
                                let msg = err.trim_matches('"').to_string();
                                return TaskResult::ForkSessionFailed {
                                    agent_id,
                                    error: sanitize_user_error(&format!("fork failed: {msg}")),
                                };
                            }
                            match crate::app::session_startup::fork_response_new_session_id(
                                resp.0.get(),
                            ) {
                                Some(sid) => {
                                    TaskResult::ForkSessionReady {
                                        agent_id,
                                        new_session_id: acp::SessionId::new(sid),
                                        cwd: parent_cwd,
                                        parent_session_id,
                                    }
                                }
                                None => {
                                    TaskResult::ForkSessionFailed {
                                        agent_id,
                                        error: "fork response missing newSessionId".into(),
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            TaskResult::ForkSessionFailed {
                                agent_id,
                                error: sanitize_user_error(&format!("fork failed: {e}")),
                            }
                        }
                    }
                });
        }
        Effect::HydrateSessionMetaFromDisk {
            agent_id,
            session_id,
            cwd,
            last_turn_summary_gen,
        } => {
            tasks
                .spawn(async move {
                    let info = xai_grok_shell::session::info::Info {
                        id: session_id,
                        cwd: cwd.to_string_lossy().to_string(),
                    };
                    let path = xai_grok_shell::session::persistence::session_dir(&info)
                        .join("summary.json");
                    type DiskTitle = (Option<(String, bool)>, Option<String>);
                    let (title, last_turn_summary) = tokio::task::spawn_blocking(move || -> Option<
                            DiskTitle,
                        > {
                            let raw = std::fs::read_to_string(path).ok()?;
                            let summary: xai_grok_shell::session::persistence::Summary = serde_json::from_str(
                                    &raw,
                                )
                                .ok()?;
                            let manual = summary.manual_title_opt();
                            let is_manual = manual.is_some();
                            let title = manual
                                .or_else(|| summary.display_title_opt())
                                .map(|t| (t, is_manual));
                            Some((title, summary.last_turn_summary))
                        })
                        .await
                        .ok()
                        .flatten()
                        .unwrap_or((None, None));
                    TaskResult::SessionMetaFromDisk {
                        agent_id,
                        title,
                        last_turn_summary,
                        last_turn_summary_gen,
                    }
                });
        }
        Effect::FetchBilling { agent_id, silent, nonce } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    use xai_grok_shell::extensions::billing::BillingConfigResponse;
                    let req = acp::ExtRequest::new(
                        "x.ai/billing",
                        serde_json::value::to_raw_value(&serde_json::json!({}))
                            .expect("serialize billing params")
                            .into(),
                    );
                    let parsed = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let result = wrapper.get("result").unwrap_or(&wrapper);
                            serde_json::from_value::<
                                BillingConfigResponse,
                            >(result.clone())
                        }
                        Err(e) => {
                            return TaskResult::BillingError {
                                agent_id,
                                error: sanitize_user_error(&format!("{e}")),
                                silent,
                                nonce,
                            };
                        }
                    };
                    let billing = match parsed {
                        Ok(billing) => billing,
                        Err(e) => {
                            return TaskResult::BillingError {
                                agent_id,
                                error: format!("Parse error: {e}"),
                                silent,
                                nonce,
                            };
                        }
                    };
                    let subscription_tier = billing.subscription_tier;
                    let balance = billing.config.map(credit_balance_from_config);
                    let autotopup = if has_prepaid_credits(balance.as_ref()) {
                        fetch_auto_topup_info(&tx).await
                    } else {
                        crate::views::credit_bar::AutoTopupFetch::Cleared
                    };
                    TaskResult::BillingFetched {
                        agent_id,
                        balance,
                        silent,
                        subscription_tier,
                        autotopup,
                        nonce,
                    }
                });
        }
        Effect::RefreshGate => {
            tasks
                .spawn(async move {
                    let settings = tokio::task::spawn_blocking(|| {
                            if !xai_grok_shell::util::config::resolve_remote_fetch_enabled() {
                                return None;
                            }
                            let grok_home = xai_grok_shell::util::grok_home::grok_home();
                            let store = xai_grok_login::read_auth_json(
                                    &grok_home.join("auth.json"),
                                )
                                .ok()?;
                            let scope = xai_grok_login::GrokComConfig::default()
                                .auth_scope();
                            let auth = xai_grok_login::lookup_auth(&store, &scope)?;
                            let proxy_base = std::env::var(
                                    "GROK_CLI_CHAT_PROXY_BASE_URL",
                                )
                                .unwrap_or_else(|_| {
                                    xai_grok_shell::agent::config::CLI_CHAT_PROXY_BASE_URL_DEFAULT
                                        .to_owned()
                                });
                            xai_grok_shell::remote::fetch_settings_blocking(
                                    &proxy_base,
                                    &auth,
                                    None,
                                )
                                .into_option()
                        })
                        .await
                        .ok()
                        .flatten();
                    TaskResult::GateRefreshed {
                        settings,
                    }
                });
        }
        Effect::FetchAppBilling { nonce } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    use xai_grok_shell::extensions::billing::BillingConfigResponse;
                    let req = acp::ExtRequest::new(
                        "x.ai/billing",
                        serde_json::value::to_raw_value(&serde_json::json!({}))
                            .expect("serialize billing params")
                            .into(),
                    );
                    let resp = match acp_send(req, &tx).await {
                        Ok(resp) => resp,
                        Err(e) => {
                            return TaskResult::AppBillingError {
                                error: sanitize_user_error(&format!("{e}")),
                                nonce,
                            };
                        }
                    };
                    let wrapper: serde_json::Value = serde_json::from_str(resp.0.get())
                        .unwrap_or_default();
                    let result = wrapper.get("result").unwrap_or(&wrapper);
                    let billing = match serde_json::from_value::<
                        BillingConfigResponse,
                    >(result.clone()) {
                        Ok(billing) => billing,
                        Err(e) => {
                            return TaskResult::AppBillingError {
                                error: format!("Parse error: {e}"),
                                nonce,
                            };
                        }
                    };
                    let balance = billing.config.map(credit_balance_from_config);
                    let autotopup = if has_prepaid_credits(balance.as_ref()) {
                        fetch_auto_topup_info(&tx).await
                    } else {
                        crate::views::credit_bar::AutoTopupFetch::Cleared
                    };
                    TaskResult::AppBillingFetched {
                        balance,
                        autotopup,
                        nonce,
                    }
                });
        }
        Effect::DebounceSuggestions { agent_id, generation } => {
            tasks
                .spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    TaskResult::SuggestionDebounceExpired {
                        agent_id,
                        generation,
                    }
                });
        }
        Effect::DebouncePluginCta { agent_id, generation } => {
            tasks
                .spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    TaskResult::PluginCtaDebounceExpired {
                        agent_id,
                        generation,
                    }
                });
        }
        Effect::FetchShellSuggestions {
            agent_id,
            text,
            cursor,
            cwd,
            generation,
            limit,
            include_ai,
            ai_model,
            session_id,
            token_only,
        } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    // By reference: echoed back below as `request_text`.
                    "text": &text,
                    "cursor": cursor,
                    "cwd": cwd,
                    "includeAi": include_ai,
                    "aiModel": ai_model,
                    "sessionId": session_id,
                    "limit": limit,
                    "generation": generation,
                    "tokenOnly": token_only,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/suggest",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize suggest params")
                            .into(),
                    );
                    match acp_send(req, &tx).await {
                        Ok(resp) => {
                            let wrapper: serde_json::Value = serde_json::from_str(
                                    resp.0.get(),
                                )
                                .unwrap_or_default();
                            let parsed = crate::views::suggestion_controller::SuggestResponseParsed::from_json(
                                &wrapper,
                            );
                            match parsed {
                                Some(response) => {
                                    TaskResult::ShellSuggestionsLoaded {
                                        agent_id,
                                        response,
                                        request_text: text,
                                        request_cursor: cursor,
                                    }
                                }
                                None => TaskResult::CancelComplete,
                            }
                        }
                        Err(_) => TaskResult::CancelComplete,
                    }
                });
        }
        Effect::FetchPromptSuggestion { agent_id, generation, model, session_id } => {
            let tx = acp_tx.clone();
            tasks
                .spawn(async move {
                    let params = serde_json::json!({
                    "generation": generation,
                    "model": model,
                    "sessionId": session_id,
                });
                    let req = acp::ExtRequest::new(
                        "x.ai/suggestPrompt",
                        serde_json::value::to_raw_value(&params)
                            .expect("serialize suggestPrompt params")
                            .into(),
                    );
                    let suggestion = match acp_send(req, &tx).await {
                        Ok(resp) => {
                            serde_json::from_str::<serde_json::Value>(resp.0.get())
                                .ok()
                                .as_ref()
                                .map(|v| v.get("result").unwrap_or(v))
                                .and_then(|r| r.get("suggestion"))
                                .and_then(|s| s.as_str())
                                .map(str::to_owned)
                        }
                        Err(_) => None,
                    };
                    TaskResult::PromptSuggestionLoaded {
                        agent_id,
                        suggestion,
                        generation,
                    }
                });
        }
    }
    (false, meta)
}
/// Fetch session info from ACP via `x.ai/session/info`.
async fn fetch_session_info(
    session_id: &acp::SessionId,
    tx: &AcpAgentTx,
) -> Result<SessionInfoResponse, String> {
    let request = acp::ExtRequest::new(
        "x.ai/session/info",
        serde_json::value::to_raw_value(
                &serde_json::json!({
            "sessionId": session_id.0.to_string()
        }),
            )
            .expect("serialize session/info params")
            .into(),
    );
    let resp = acp_send(request, tx)
        .await
        .map_err(|e| sanitize_user_error(&format!("couldn't fetch session info: {e}")))?;
    let envelope: ExtMethodResult<SessionInfoResponse> = serde_json::from_str(
            resp.0.get(),
        )
        .map_err(|e| {
            tracing::debug!("typed session info deser failed: {e}");
            "invalid session info response".to_string()
        })?;
    if let Some(err) = envelope.error {
        let msg = err.as_str().map(String::from).unwrap_or_else(|| err.to_string());
        return Err(msg);
    }
    envelope.result.ok_or_else(|| "session info response missing result".to_string())
}
/// Fetch [`PromptUsage`] via `x.ai/session/usage` (bare response, no envelope).
async fn fetch_session_usage(
    session_id: &acp::SessionId,
    tx: &AcpAgentTx,
) -> Result<xai_grok_shell::extensions::notification::PromptUsage, String> {
    let request = acp::ExtRequest::new(
        "x.ai/session/usage",
        serde_json::value::to_raw_value(
                &serde_json::json!({
            "sessionId": session_id.0.to_string()
        }),
            )
            .expect("serialize session/usage params")
            .into(),
    );
    let resp = acp_send(request, tx).await.map_err(unsupported_or_sanitized)?;
    let parsed: xai_grok_shell::extensions::usage::SessionUsageResponse = serde_json::from_str(
            resp.0.get(),
        )
        .map_err(|e| {
            tracing::debug!("session usage deser failed: {e}");
            "invalid session usage response".to_string()
        })?;
    Ok(parsed.usage)
}
/// An agent that predates an extension answers `method_not_found`
fn unsupported_or_sanitized(e: acp::Error) -> String {
    if i32::from(e.code) == i32::from(acp::Error::method_not_found().code) {
        "not supported by this agent version".to_string()
    } else {
        sanitize_user_error(&e.to_string())
    }
}
/// Shared `x.ai/session/rename` RPC for rename and `/rename --auto`.
async fn session_rename_rpc(
    tx: &AcpAgentTx,
    request: actions::RenameSessionRequest,
) -> Result<(), String> {
    let verb = if request.reset_to_auto {
        "reset session title"
    } else {
        "rename session"
    };
    let ext = acp::ExtRequest::new(
        "x.ai/session/rename",
        serde_json::value::to_raw_value(&request)
            .expect("serialize rename params")
            .into(),
    );
    match acp_send(ext, tx).await {
        Ok(resp) => {
            let wrapper: serde_json::Value = serde_json::from_str(resp.0.get())
                .unwrap_or_default();
            if let Some(err) = wrapper.get("error").filter(|v| !v.is_null()) {
                let msg = err
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| err.to_string());
                return Err(msg);
            }
            Ok(())
        }
        Err(e) => Err(sanitize_user_error(&format!("couldn't {verb}: {e}"))),
    }
}
/// Session title from local persistence: loads only this session's summary, never the all-sessions list.
/// `cwd` comes from the `x.ai/session/info` response.
async fn lookup_session_title(session_id: &acp::SessionId, cwd: &str) -> Option<String> {
    lookup_session_title_in(
            xai_grok_shell::util::grok_home::grok_home(),
            session_id,
            cwd,
        )
        .await
}
/// [`lookup_session_title`] against an explicit root, for tests.
async fn lookup_session_title_in(
    root: std::path::PathBuf,
    session_id: &acp::SessionId,
    cwd: &str,
) -> Option<String> {
    use xai_grok_shell::session::storage::{JsonlStorageAdapter, StorageAdapter};
    let info = xai_grok_shell::session::info::Info {
        id: session_id.clone(),
        cwd: cwd.to_string(),
    };
    JsonlStorageAdapter::with_root(root)
        .load_summary(&info)
        .await
        .ok()
        .and_then(|s| s.display_title_opt())
}
/// Format session info into a human-readable string.
/// Mirrors the TUI's `render_session_info` for pager display.
/// Structured `/session-info` rows: the single source of truth for both the formatted string ([`format_session_info`]) and the modal.
fn session_info_fields(
    info: &SessionInfoResponse,
    title: Option<&str>,
    show_resolved_model: bool,
) -> Vec<SessionInfoField> {
    let mut fields = Vec::new();
    let mut push = |label: &'static str, value: String, compact: bool| {
        fields
            .push(SessionInfoField {
                label,
                value,
                compact,
            });
    };
    if let Some(t) = title {
        push("Title", t.to_string(), false);
    }
    push(
        "Shell version",
        xai_grok_version::display_version(xai_grok_update::channel_label()),
        false,
    );
    push("Session ID", info.session_id.to_string(), false);
    if let Some(id) = info.data.conversation_id.as_deref().filter(|id| !id.is_empty()) {
        push("Conversation ID", id.to_string(), false);
    }
    push("Working directory", info.cwd.to_string(), false);
    let model = info.data.model.as_deref().unwrap_or("unknown");
    let model_display = xai_grok_shell::session::model_display_name(
        info.data.model_display_name.as_deref(),
        model,
        info.data.resolved_model_id.as_deref(),
        show_resolved_model,
    );
    push("Model", model_display.to_string(), true);
    if info.data.show_model_fingerprint
        && let Some(fp) = info.data.model_fingerprint.as_deref()
    {
        push("Model Hash", fp.to_string(), true);
    }
    if let Some(b) = info.data.api_backend.as_deref() {
        push("API Backend", b.to_string(), true);
    }
    if let Some(profile) = xai_grok_sandbox::profile_name() {
        push("Sandbox", profile.to_string(), true);
    }
    push("Turn", info.data.turn_index.to_string(), true);
    let ctx = &info.data.context;
    push(
        "Context",
        format!("{} / {} tokens ({}%)", ctx.used, ctx.total, ctx.usage_pct),
        true,
    );
    fields
}
/// The `/session-info` block as a plain string for minimal-mode scrollback.
/// Built from [`session_info_fields`] (one `  Label: value` line each) with the auth prose spliced in after the shell version.
/// That keeps it a single source of truth with the modal.
fn format_session_info(
    info: &SessionInfoResponse,
    title: Option<&str>,
    show_resolved_model: bool,
    is_api_key_auth: bool,
    api_key_env_set: bool,
) -> String {
    let auth_lines = format_auth_lines(is_api_key_auth, api_key_env_set);
    let mut out = String::new();
    for field in session_info_fields(info, title, show_resolved_model) {
        out.push_str("  ");
        out.push_str(field.label);
        out.push_str(": ");
        out.push_str(&field.value);
        out.push('\n');
        if field.label == "Shell version" {
            out.push_str(&auth_lines);
        }
    }
    out.truncate(out.trim_end_matches('\n').len());
    out
}
/// Auth section for `/session-info`: active login method.
///
/// This reflects the process login / ACP auth method, not per-model sampling credentials (a model `api_key`/`env_key` can still own the turn).
fn format_auth_lines(is_api_key_auth: bool, api_key_env_set: bool) -> String {
    if is_api_key_auth {
        let method = if api_key_env_set {
            "  Auth method: API key (XAI_API_KEY)\n"
        } else {
            "  Auth method: API key\n"
        };
        return format!(
            "{method}  Run `grok login` to use your SuperGrok subscription instead.\n"
        );
    }
    String::from("  Auth method: OAuth\n")
}
/// Session replay then restyles the echo exactly like the composer highlighted it at submit time.
/// This producer never combines them with a `displayText` override, and the tracker ignores them when one is present.
/// Empty ranges keep `meta: None`, so the legacy wire shape stays byte-identical.
fn plain_prompt_content_block(
    text: String,
    skill_token_ranges: &[std::ops::Range<usize>],
) -> acp::ContentBlock {
    let meta = if skill_token_ranges.is_empty() {
        None
    } else {
        let ranges: Vec<serde_json::Value> = skill_token_ranges
            .iter()
            .map(|r| serde_json::json!([r.start, r.end]))
            .collect();
        let mut map = acp::Meta::new();
        map.insert(
            crate::acp::meta::user_prompt_meta::SKILL_TOKEN_RANGES.into(),
            serde_json::Value::Array(ranges),
        );
        Some(map)
    };
    acp::ContentBlock::Text(acp::TextContent::new(text).meta(meta))
}
/// Build the `PromptRequest._meta` payload: `promptId` for notification / response correlation, plus `screenMode`.
/// `screenMode` is `fullscreen` | `inline` | `minimal` (headless stamps `"headless"` in its own path).
/// `screen_mode` is `None` only under `SessionFlags::default()` (tests); the key is omitted then, keeping the legacy wire shape byte-identical.
fn prompt_request_meta(
    prompt_id: &str,
    screen_mode: Option<&'static str>,
) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("promptId".into(), serde_json::Value::String(prompt_id.into()));
    if let Some(mode) = screen_mode {
        map.insert("screenMode".into(), serde_json::Value::String(mode.into()));
    }
    serde_json::Value::Object(map)
}
/// Build the `session/cancel` `_meta`, shared by the TUI cancel effect and the headless fail-safe so the wire shape has one owner.
/// The rewind is a request, not a command: the shell re-checks rewindable and prompt identity.
pub(crate) fn cancel_notification_meta(
    cancel_subagents: bool,
    trigger: Option<&str>,
    rewind_prompt_id: Option<&str>,
) -> acp::Meta {
    let mut meta = acp::Meta::new();
    meta.insert("cancelSubagents".into(), cancel_subagents.into());
    if let Some(trigger) = trigger {
        meta.insert(
            crate::app::turn_completion::CANCEL_TRIGGER_KEY.into(),
            trigger.into(),
        );
    }
    if let Some(pid) = rewind_prompt_id {
        meta.insert("rewindIfNoOutput".into(), true.into());
        meta.insert("rewindIfPristine".into(), true.into());
        meta.insert("promptId".into(), pid.into());
    }
    meta
}
pub(crate) const REWIND_MODE_WIRE: &str = "conversation_only";
pub(crate) fn rewind_execute_params(
    session_id: &str,
    target_prompt_index: usize,
) -> serde_json::Value {
    serde_json::json!({
        "sessionId": session_id,
        "targetPromptIndex": target_prompt_index,
        "force": true,
        "mode": REWIND_MODE_WIRE,
    })
}
/// The shell prefers the `content` text block over `question`. The omit notice
/// has to be on both, or a partial drop never reaches the model.
fn append_btw_notice(
    question: &mut String,
    blocks: Option<&mut Vec<acp::ContentBlock>>,
    notice: &str,
) {
    question.push_str("\n\n");
    question.push_str(notice);
    let Some(blocks) = blocks else {
        return;
    };
    if let Some(acp::ContentBlock::Text(text)) = blocks
        .iter_mut()
        .find(|block| matches!(block, acp::ContentBlock::Text(_)))
    {
        text.text.push_str("\n\n");
        text.text.push_str(notice);
    }
}
/// Unlinks `/btw` image files when the encode task finishes, panics, or is dropped.
struct BtwSendImages(Vec<crate::prompt_images::PastedImage>);
impl Drop for BtwSendImages {
    fn drop(&mut self) {
        crate::prompt_images::drain_and_cleanup(
            crate::prompt_images::SessionPathPolicy::Delete,
            &mut self.0,
        );
    }
}
fn btw_image_notice(omitted: usize, attached: usize) -> Option<String> {
    if omitted == 0 {
        return None;
    }
    Some(
        if omitted == attached {
            "Attached images were not included (too large or could not be loaded)."
                .to_string()
        } else {
            format!(
            "{omitted} attached image(s) were not included (over the 50MB side-question limit or could not be loaded)."
        )
        },
    )
}
/// Build the `x.ai/btw` params.
/// `content` is omitted when `None` so a text-only side question stays byte-identical on the wire.
#[expect(
    clippy::expect_used,
    reason = "ACP content blocks are always JSON-serializable; failure is a bug"
)]
fn build_btw_params(
    session_id: &acp::SessionId,
    question: &str,
    blocks: Option<&[acp::ContentBlock]>,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "sessionId": session_id.0.to_string(),
        "question": question,
    });
    if let Some(blocks) = blocks && let Some(object) = params.as_object_mut() {
        object
            .insert(
                "content".to_owned(),
                serde_json::to_value(blocks).expect("serialize btw content"),
            );
    }
    params
}
/// Send interjections oldest-first in one task so ACP order matches the effect vector.
pub(crate) fn spawn_ordered_interjects(
    tasks: &mut JoinSet<TaskResult>,
    acp_tx: &AcpAgentTx,
    agent_id: crate::app::agent::AgentId,
    session_id: acp::SessionId,
    items: Vec<(String, String, Option<Vec<acp::ContentBlock>>)>,
) {
    let tx = acp_tx.clone();
    tasks
        .spawn(async move {
            let mut pending: std::collections::VecDeque<_> = items.into();
            while let Some((text, interjection_id, blocks)) = pending.pop_front() {
                let params = build_interject_params(
                    &session_id,
                    &text,
                    &interjection_id,
                    blocks.as_deref(),
                );
                let request = acp::ExtRequest::new(
                    "x.ai/interject",
                    serde_json::value::to_raw_value(&params)
                        .expect("serialize interject params")
                        .into(),
                );
                if let Err(e) = acp_send(request, &tx).await {
                    let mut leftover = vec![(text, interjection_id, blocks)];
                    leftover.extend(pending);
                    return TaskResult::InterjectFailed {
                        agent_id,
                        error: sanitize_user_error(
                            &format!("couldn't send interjection: {e}"),
                        ),
                        remaining: leftover,
                    };
                }
            }
            TaskResult::InterjectQueued {
                agent_id,
            }
        });
}
/// Consume a leading `SendInterject` plus consecutive same-session ones; spawn them in order.
/// Returns `Some(effect)` when `first` was not an interject.
pub(crate) fn take_coalesced_interjects(
    first: Effect,
    rest: &mut std::iter::Peekable<impl Iterator<Item = Effect>>,
    tasks: &mut JoinSet<TaskResult>,
    acp_tx: &AcpAgentTx,
) -> Option<Effect> {
    let Effect::SendInterject { agent_id, session_id, text, interjection_id, blocks } = first
    else {
        return Some(first);
    };
    let mut items = vec![(text, interjection_id, blocks)];
    while let Some(
        Effect::SendInterject { agent_id: next_agent, session_id: next_session, .. },
    ) = rest.peek()
    {
        if *next_agent != agent_id || *next_session != session_id {
            break;
        }
        let Some(Effect::SendInterject { text, interjection_id, blocks, .. }) = rest
            .next() else {
            break;
        };
        items.push((text, interjection_id, blocks));
    }
    spawn_ordered_interjects(tasks, acp_tx, agent_id, session_id, items);
    None
}
/// Build the `x.ai/interject` params.
/// The optional structured `content` (text and images) is omitted ENTIRELY when `None` so the legacy wire shape stays byte-identical.
/// Extracted from the spawn for testability.
fn build_interject_params(
    session_id: &acp::SessionId,
    text: &str,
    interjection_id: &str,
    blocks: Option<&[acp::ContentBlock]>,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "sessionId": session_id.0.to_string(),
        "text": text,
        "interjectionId": interjection_id,
    });
    if let Some(blocks) = blocks && let Some(obj) = params.as_object_mut() {
        obj.insert(
            "content".into(),
            serde_json::to_value(blocks).expect("serialize interject content"),
        );
    }
    params
}
#[cfg(test)]
mod tests;
