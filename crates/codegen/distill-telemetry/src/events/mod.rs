// Modified for Distill by Samuel Fajreldines, 2026.
//! Telemetry event structs. Every struct needs a `telemetry_event!` binding.
//! `log_event` auto-injects `session_id`/`turn_number` and reserves every key in `client::RESERVED_EVENT_KEYS`.
//!
//! Extracted from `distill-shell` so binaries (TUI, sampler) can reuse them without the shell's HTTP product-analytics client.

use serde::Serialize;

pub use super::enums::PrCreationSource;

mod active_agent_message;
mod auth;
mod cli_update;
mod clone;
mod compaction;
mod consent;
mod dashboard;
mod errors;
mod extensions;
mod external_otel;
mod feedback;
mod git;
mod hooks;
mod mcp;
mod media;
mod memory;
mod model;
mod permission;
mod permission_analytics;
mod plan_mode;
mod plugin;
mod process;
mod prompt;
mod redirect;
mod session;
mod skills;
mod slash;
mod startup;
mod status_line;
mod subagent;
mod terminal;
mod tips;
mod tool;
mod turn;
mod upsell;
mod yolo;
pub use active_agent_message::*;
pub use auth::*;
pub use cli_update::*;
pub use clone::*;
pub use compaction::*;
pub use consent::*;
pub use dashboard::*;
pub use errors::*;
pub use extensions::*;
pub use external_otel::*;
pub use feedback::*;
pub use git::*;
pub use hooks::*;
pub use mcp::*;
pub use media::*;
pub use memory::*;
pub use model::*;
pub use permission::*;
pub use permission_analytics::*;
pub use plan_mode::*;
pub use plugin::*;
pub use process::*;
pub use prompt::*;
pub use redirect::*;
pub use session::*;
pub use skills::*;
pub use slash::*;
pub use startup::*;
pub use status_line::*;
pub use subagent::*;
pub use terminal::*;
pub use tips::*;
pub use tool::*;
pub use turn::*;
pub use upsell::*;
pub use yolo::*;

pub trait TelemetryEvent: Serialize + Send + 'static {
    const NAME: &'static str;

    /// Curated external-OTEL representation (see [`crate::external`]). Default: not exported externally. Override via the
    /// macro's `external = …` arm. The mapping functions live together in `external/schema.rs` so the whole wire schema is
    /// one reviewable file.
    fn external_record(&self) -> Option<crate::external::schema::ExternalRecord> {
        None
    }
}

macro_rules! telemetry_event {
    ($struct:path, $name:literal) => {
        impl $crate::events::TelemetryEvent for $struct {
            const NAME: &'static str = $name;
        }
    };
    ($struct:path, $name:literal, external = $mapper:path) => {
        impl $crate::events::TelemetryEvent for $struct {
            const NAME: &'static str = $name;

            fn external_record(&self) -> Option<$crate::external::schema::ExternalRecord> {
                $mapper(self)
            }
        }
    };
}

// ─────────────────────────────────────────────────────────────────────────────
// Event name bindings
// ─────────────────────────────────────────────────────────────────────────────

telemetry_event!(ManualAuth, "manual_auth");
telemetry_event!(AuthLockWait, "auth_lock_wait");
telemetry_event!(AuthLockTimeout, "auth_lock_timeout");
telemetry_event!(
    AuthLockReplacedOutFromUnder,
    "auth_lock_replaced_out_from_under"
);
telemetry_event!(CliUpdate, "cli_update");
telemetry_event!(CloneEnded, "clone_ended");
telemetry_event!(WorktreeEnded, "worktree_ended");
telemetry_event!(RedirectApplied, "redirect_applied");
telemetry_event!(RedirectFixupFailed, "redirect_fixup_failed");
telemetry_event!(RedirectDemoted, "redirect_demoted");
telemetry_event!(RedirectOverwrite, "redirect_overwrite");
telemetry_event!(RedirectLimitHit, "redirect_limit_hit");

telemetry_event!(Login, "login", external = crate::external::schema::map_auth);
telemetry_event!(LoginPickerShown, "login_picker_shown");
telemetry_event!(LoginMethodChosen, "login_method_chosen");
telemetry_event!(LoginCompleted, "login_completed");
telemetry_event!(LoginFailed, "login_failed");
telemetry_event!(LoginAbandoned, "login_abandoned");
telemetry_event!(ApiKeySaveResult, "api_key_save_result");
telemetry_event!(
    PlanModeToggled,
    "plan_mode_toggled",
    external = crate::external::schema::map_plan_mode_toggled
);
telemetry_event!(
    ContextualTip,
    "contextual_tip",
    external = crate::external::schema::map_contextual_tip
);
telemetry_event!(PromptSuggestion, "prompt_suggestion");
telemetry_event!(
    YoloToggled,
    "yolo_toggled",
    external = crate::external::schema::map_yolo_toggled
);
telemetry_event!(SlashCommandUsed, "slash_command_used");
telemetry_event!(PermissionPrompted, "permission_prompted");
telemetry_event!(
    PermissionDecisionRecord,
    "permission_decision",
    external = crate::external::schema::map_tool_decision
);
telemetry_event!(AutoCompactFired, "auto_compact_fired");
telemetry_event!(CompactionTriggered, "compaction_triggered");
telemetry_event!(
    CompactionCompleted,
    "compaction_completed",
    external = crate::external::schema::map_compaction
);
telemetry_event!(AutoCompactSuppressed, "auto_compact_suppressed");
telemetry_event!(CompactionRetryDegraded, "compaction_retry_degraded");
telemetry_event!(
    SubagentLaunched,
    "subagent_launched",
    external = crate::external::schema::map_subagent_launched
);
telemetry_event!(
    SubagentCompleted,
    "subagent_completed",
    external = crate::external::schema::map_subagent_completed
);
telemetry_event!(SubagentLimitHit, "subagent_limit_hit");
telemetry_event!(SubagentRateLimitWaited, "subagent_rate_limit_waited");
telemetry_event!(
    ActiveAgentMessageCompleted,
    "active_agent_message_completed"
);
telemetry_event!(ActiveAgentMessageLimitHit, "active_agent_message_limit_hit");
telemetry_event!(ActiveAgentMessageQuotaHit, "active_agent_message_quota_hit");
telemetry_event!(ActiveAgentMessageSettled, "active_agent_message_settled");
telemetry_event!(WorkflowRunStarted, "workflow_run_started");
telemetry_event!(WorkflowRunEnded, "workflow_run_ended");
telemetry_event!(
    ModelSwitched,
    "model_switched",
    external = crate::external::schema::map_model_switched
);
telemetry_event!(PluginAdded, "plugin_added");
telemetry_event!(PluginRemoved, "plugin_removed");
telemetry_event!(
    PluginInstalled,
    "plugin_installed",
    external = crate::external::schema::map_plugin_installed
);
telemetry_event!(PluginUninstalled, "plugin_uninstalled");
telemetry_event!(PluginReloaded, "plugin_reloaded");
telemetry_event!(
    PluginUsed,
    "plugin_used",
    external = crate::external::schema::map_plugin_used
);
telemetry_event!(PluginCtaImpression, "plugin_cta_impression");
telemetry_event!(PluginCtaConnectClicked, "plugin_cta_connect_clicked");
telemetry_event!(PluginCtaDismissed, "plugin_cta_dismissed");
telemetry_event!(PluginCtaInstalled, "plugin_cta_installed");
telemetry_event!(ExtensionsModalOpened, "extensions_modal_opened");
telemetry_event!(ExtensionsModalAction, "extensions_modal_action");
telemetry_event!(HookAdded, "hook_added");
telemetry_event!(HookRemoved, "hook_removed");
telemetry_event!(HookTrusted, "hook_trusted");
telemetry_event!(HookExecuted, "hook_executed");
telemetry_event!(HookBlocked, "hook_blocked");
telemetry_event!(ClientHookGate, "client_hook_gate");
telemetry_event!(SkillAdded, "skill_added");
telemetry_event!(SkillRemoved, "skill_removed");
telemetry_event!(
    SkillDispatched,
    "skill_dispatched",
    external = crate::external::schema::map_skill_activated
);
telemetry_event!(
    McpServerConnected,
    "mcp_server_connected",
    external = crate::external::schema::map_mcp_server_connected
);
telemetry_event!(
    McpServerFailed,
    "mcp_server_failed",
    external = crate::external::schema::map_mcp_server_failed
);
telemetry_event!(McpInitCompleted, "mcp_init_completed");
telemetry_event!(McpToolCalled, "mcp_tool_called");
telemetry_event!(
    SessionHarness,
    "session_harness",
    external = crate::external::schema::map_session_start
);
telemetry_event!(SessionLoad, "session_load");
telemetry_event!(
    SessionNew,
    "session_new",
    external = crate::external::schema::map_session_new
);
telemetry_event!(
    SessionCreateFailed,
    "session_create_failed",
    external = crate::external::schema::map_session_create_failed
);
telemetry_event!(
    PromptSubmitted,
    "prompt_submitted",
    external = crate::external::schema::map_user_prompt
);
telemetry_event!(UserFeedback, "user_feedback");
telemetry_event!(FeedbackModalOpened, "feedback_modal_opened");
telemetry_event!(FeedbackDraftOp, "feedback_draft_op");
telemetry_event!(RolloutSurvey, "rollout_survey");
telemetry_event!(PrCreated, "pr_created");
telemetry_event!(PrMerged, "pr_merged");
telemetry_event!(MultiAgentFollowup, "multi_agent_followup");
telemetry_event!(MultiAgentApply, "multi_agent_apply");
telemetry_event!(MultiAgentDiscard, "multi_agent_discard");
telemetry_event!(RepoChanges, "repo_changes");
telemetry_event!(NonGitDecisionEvent, "non_git_decision");
telemetry_event!(
    PromptLatency,
    "prompt_latency",
    external = crate::external::schema::map_prompt_latency
);
telemetry_event!(CancellationCompleted, "cancellation_completed");
telemetry_event!(HeapThresholdCrossed, "heap_threshold_crossed");
telemetry_event!(ProcessResourceUsage, "process_resource_usage");
telemetry_event!(ProcessResourceLimits, "process_resource_limits");
telemetry_event!(
    TurnCompleted,
    "turn_completed",
    external = crate::external::schema::map_turn_completed
);
telemetry_event!(ShellTrueNoop, "shell_true_noop");
telemetry_event!(ActionStationarityNudge, "action_stationarity_nudge");
telemetry_event!(ActionStationarityStop, "action_stationarity_stop");
telemetry_event!(
    ToolCallCompleted,
    "tool_call_completed",
    external = crate::external::schema::map_tool_result
);
telemetry_event!(
    ModelResponseReceived,
    "model_response_received",
    external = crate::external::schema::map_api_request
);
telemetry_event!(
    AssistantResponse,
    "assistant_response",
    external = crate::external::schema::map_assistant_response
);
telemetry_event!(MemoryFlushed, "memory_flushed");
telemetry_event!(MediaGenerated, "media_generated");
telemetry_event!(
    SessionEnded,
    "session_ended",
    external = crate::external::schema::map_session_end
);
telemetry_event!(SessionEndTimings, "session_end_timings");
telemetry_event!(
    AgentConnect,
    "agent_connect",
    external = crate::external::schema::map_agent_connect
);
telemetry_event!(
    StartupCompleted,
    "startup_completed",
    external = crate::external::schema::map_startup_completed
);
telemetry_event!(
    StartupInteractive,
    "startup_interactive",
    external = crate::external::schema::map_startup_interactive
);
telemetry_event!(
    StartupSubTimers,
    "startup_subtimers",
    external = crate::external::schema::map_startup_sub_timers
);
telemetry_event!(PagerSlashCommand, "pager_slash_command");
telemetry_event!(PlanSubmit, "plan_submit");
telemetry_event!(EventLoopStall, "event_loop_stall");
telemetry_event!(TermWriterBlocked, "term_writer_blocked");
telemetry_event!(PromptAckTimeoutFired, "prompt_ack_timeout_fired");
telemetry_event!(SuperGrokUpsellShown, "supergrok_upsell_shown");
telemetry_event!(SuperGrokUpsellClicked, "supergrok_upsell_clicked");
telemetry_event!(AnnouncementCtaShown, "announcement_cta_shown");
telemetry_event!(AnnouncementCtaClicked, "announcement_cta_clicked");
telemetry_event!(CodingDataConsentSelected, "coding_data_consent_selected");
telemetry_event!(FeedbackTraceCardShown, "feedback_trace_card_shown");
telemetry_event!(
    FeedbackTraceConsentSelected,
    "feedback_trace_consent_selected"
);
telemetry_event!(TerminalTelemetry, "terminal_context");
telemetry_event!(DisplayRefreshProbe, "display_refresh_probe");
telemetry_event!(BackspaceNoEffect, "backspace_no_effect");
telemetry_event!(ClipboardImagePaste, "clipboard_image_paste");
telemetry_event!(ClipboardPasteProbeDropped, "clipboard_paste_probe_dropped");
telemetry_event!(PasteKeyEmptyHostClipboard, "paste_key_empty_host_clipboard");
telemetry_event!(ClipboardCopy, "clipboard_copy");
telemetry_event!(NotificationEmitted, "notification_emitted");
telemetry_event!(DashboardOpened, "dashboard_opened");
telemetry_event!(DashboardClosed, "dashboard_closed");
telemetry_event!(DashboardAgentAttached, "dashboard_agent_attached");
telemetry_event!(DashboardAgentLaunched, "dashboard_agent_launched");
telemetry_event!(BlockViewerOpened, "block_viewer_opened");
telemetry_event!(BlockViewerQuoted, "block_viewer_quoted");
telemetry_event!(ShortcutUsed, "shortcut_used");
telemetry_event!(
    RateLimitHit,
    "rate_limit_hit",
    external = crate::external::schema::map_rate_limit_hit
);
telemetry_event!(CreditLimitHit, "credit_limit_hit");
telemetry_event!(CreditLimitUpsellShown, "credit_limit_upsell_shown");
telemetry_event!(CreditLimitUpsellClicked, "credit_limit_upsell_clicked");
telemetry_event!(SubscriptionActivated, "subscription_activated");
telemetry_event!(StatusLineConfigured, "status_line_configured");
telemetry_event!(StatusLineHealth, "status_line_health");
telemetry_event!(
    ApiError,
    "api_error",
    external = crate::external::schema::map_api_error
);
telemetry_event!(
    InternalError,
    "internal_error",
    external = crate::external::schema::map_internal_error
);
telemetry_event!(ExternalOtelConfigured, "external_otel_configured");
telemetry_event!(
    ExternalOtelRemotePolicyApplied,
    "external_otel_remote_policy_applied"
);
telemetry_event!(ExternalOtelExportHealth, "external_otel_export_health");

// Session lifecycle (structs in session_metrics)
telemetry_event!(crate::session_metrics::SessionStarted, "session_started");
telemetry_event!(
    crate::session_metrics::SessionContextSnapshot,
    "session_context_snapshot"
);
telemetry_event!(crate::session_metrics::Turn, "turn");
telemetry_event!(
    crate::session_metrics::TurnCompletedLifecycle,
    "turn_completed_lifecycle"
);
telemetry_event!(
    crate::session_metrics::DoomLoopDetected,
    "doom_loop_detected"
);
telemetry_event!(
    crate::session_metrics::DoomLoopRecovery,
    "doom_loop_recovery"
);
telemetry_event!(
    crate::session_metrics::TraceUploadAttempted,
    "trace_upload_attempted"
);
telemetry_event!(
    crate::session_metrics::TraceUploadSucceeded,
    "trace_upload_succeeded"
);
telemetry_event!(
    crate::session_metrics::TraceUploadSkipped,
    "trace_upload_skipped"
);
telemetry_event!(
    crate::session_metrics::TraceUploadFailed,
    "trace_upload_failed"
);

// Memory subsystem (structs in memory_telemetry)
telemetry_event!(
    crate::memory_telemetry::MemorySessionInit,
    "memory_session_init"
);
telemetry_event!(crate::memory_telemetry::MemorySearch, "memory_search");
telemetry_event!(
    crate::memory_telemetry::MemoryFlushStart,
    "memory_flush_start"
);
telemetry_event!(
    crate::memory_telemetry::MemoryFlushComplete,
    "memory_flush_complete"
);
telemetry_event!(crate::memory_telemetry::MemoryInjection, "memory_injection");
telemetry_event!(crate::memory_telemetry::MemoryReindex, "memory_reindex");
telemetry_event!(
    crate::memory_telemetry::MemoryWatcherSync,
    "memory_watcher_sync"
);
telemetry_event!(
    crate::memory_telemetry::MemorySessionSummary,
    "memory_session_summary"
);
telemetry_event!(
    crate::memory_telemetry::MemoryV2ControlsPinned,
    "memory_v2_controls_pinned"
);
telemetry_event!(
    crate::memory_telemetry::MemoryV2CaptureLifecycle,
    "memory_v2_capture_lifecycle"
);
telemetry_event!(
    crate::memory_telemetry::MemoryV2FlushResult,
    "memory_v2_flush_result"
);
telemetry_event!(
    crate::memory_telemetry::MemoryV2DreamLifecycle,
    "memory_v2_dream_lifecycle"
);
telemetry_event!(
    crate::memory_telemetry::MemoryV2GcCompleted,
    "memory_v2_gc_completed"
);
telemetry_event!(
    crate::memory_telemetry::MemoryV2CarryoverCompleted,
    "memory_v2_carryover_completed"
);
telemetry_event!(
    crate::memory_telemetry::MemoryV2Forgotten,
    "memory_v2_forgotten"
);
telemetry_event!(
    crate::memory_telemetry::MemoryV2FailClosed,
    "memory_v2_fail_closed"
);

#[cfg(test)]
mod tests {
    /// Reserved keys insert only-if-absent, so an event field that collides intentionally wins over the enrichment.
    /// Walk every registered event's fields from source and pin the intentional shadows, so a new event cannot silently shadow a reserved key.
    #[test]
    fn event_fields_shadow_reserved_keys_only_on_the_allowlist() {
        const SOURCES: &[&str] = &[
            include_str!("mod.rs"),
            include_str!("active_agent_message.rs"),
            include_str!("auth.rs"),
            include_str!("cli_update.rs"),
            include_str!("clone.rs"),
            include_str!("compaction.rs"),
            include_str!("consent.rs"),
            include_str!("dashboard.rs"),
            include_str!("errors.rs"),
            include_str!("extensions.rs"),
            include_str!("external_otel.rs"),
            include_str!("feedback.rs"),
            include_str!("git.rs"),
            include_str!("hooks.rs"),
            include_str!("mcp.rs"),
            include_str!("media.rs"),
            include_str!("memory.rs"),
            include_str!("model.rs"),
            include_str!("permission.rs"),
            include_str!("permission_analytics.rs"),
            include_str!("plan_mode.rs"),
            include_str!("plugin.rs"),
            include_str!("process.rs"),
            include_str!("prompt.rs"),
            include_str!("redirect.rs"),
            include_str!("session.rs"),
            include_str!("skills.rs"),
            include_str!("slash.rs"),
            include_str!("startup.rs"),
            include_str!("status_line.rs"),
            include_str!("subagent.rs"),
            include_str!("terminal.rs"),
            include_str!("tips.rs"),
            include_str!("tool.rs"),
            include_str!("turn.rs"),
            include_str!("upsell.rs"),
            include_str!("yolo.rs"),
            include_str!("../session/session_metrics.rs"),
            include_str!("../process/memory_telemetry.rs"),
        ];

        let mut registry: Vec<&str> = Vec::new();
        for src in SOURCES {
            for chunk in src.split("telemetry_event!(").skip(1) {
                let path = chunk
                    .trim_start()
                    .split(',')
                    .next()
                    .unwrap_or_default()
                    .trim();
                let name = path.rsplit("::").next().unwrap_or(path);
                if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    registry.push(name);
                }
            }
        }

        let mut fields: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
        for src in SOURCES {
            let mut lines = src.lines();
            while let Some(line) = lines.next() {
                let Some(decl) = line.trim_start().strip_prefix("pub struct ") else {
                    continue;
                };
                let name = decl
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or_default();
                let entry = fields.entry(name).or_default();
                if !decl.contains('{') || decl.contains('}') {
                    continue;
                }
                for body in lines.by_ref() {
                    if body == "}" {
                        break;
                    }
                    let b = body.trim_start();
                    if b.starts_with("//") || b.starts_with('#') {
                        continue;
                    }
                    let b = b.strip_prefix("pub ").unwrap_or(b);
                    if let Some((ident, _)) = b.split_once(':')
                        && !ident.is_empty()
                        && ident
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                    {
                        entry.push(ident.to_string());
                    }
                }
            }
        }

        let reserved: std::collections::BTreeSet<&str> =
            crate::client::RESERVED_EVENT_KEYS.iter().copied().collect();
        let mut shadows: std::collections::BTreeSet<(String, String)> = Default::default();
        let mut seen = std::collections::BTreeSet::new();
        for event in registry {
            assert!(seen.insert(event), "event {event} registered twice");
            let event_fields = fields
                .get(event)
                .unwrap_or_else(|| panic!("registered event {event} has no parsed struct"));
            for field in event_fields {
                if reserved.contains(field.as_str()) {
                    shadows.insert((event.to_string(), field.clone()));
                }
            }
        }
        assert!(
            seen.len() > 100,
            "the registry walk collapsed: {}",
            seen.len()
        );

        const ALLOWED: &[(&str, &str)] = &[
            ("DoomLoopDetected", "session_id"),
            ("DoomLoopDetected", "turn_number"),
            ("DoomLoopRecovery", "session_id"),
            ("DoomLoopRecovery", "turn_number"),
            ("FeedbackDraftOp", "session_id"),
            ("FeedbackModalOpened", "session_id"),
            ("MemoryFlushComplete", "session_id"),
            ("MemoryFlushStart", "session_id"),
            ("MemoryInjection", "session_id"),
            ("MemoryReindex", "session_id"),
            ("MemorySearch", "session_id"),
            ("MemorySessionInit", "session_id"),
            ("MemorySessionSummary", "session_id"),
            ("MemoryWatcherSync", "session_id"),
            ("ModelSwitched", "session_id"),
            ("NonGitDecisionEvent", "session_id"),
            ("ProcessResourceUsage", "footprint_bytes"),
            ("ProcessResourceUsage", "rss_bytes"),
            ("PromptSuggestion", "session_id"),
            ("RolloutSurvey", "session_id"),
            ("SessionHarness", "session_id"),
            ("SessionLoad", "session_id"),
            ("SessionNew", "session_id"),
            ("SessionContextSnapshot", "session_id"),
            ("SessionStarted", "session_id"),
            ("TraceUploadAttempted", "session_id"),
            ("TraceUploadAttempted", "turn_number"),
            ("TraceUploadFailed", "session_id"),
            ("TraceUploadFailed", "turn_number"),
            ("TraceUploadSkipped", "session_id"),
            ("TraceUploadSkipped", "turn_number"),
            ("TraceUploadSucceeded", "session_id"),
            ("TraceUploadSucceeded", "turn_number"),
            ("Turn", "session_id"),
            ("Turn", "turn_number"),
            // Intentional: external-stream `session.id` on the event (see `TurnCompleted`).
            ("TurnCompleted", "session_id"),
            ("TurnCompletedLifecycle", "session_id"),
            ("TurnCompletedLifecycle", "turn_number"),
            ("UserFeedback", "session_id"),
        ];
        let allowed: std::collections::BTreeSet<(String, String)> = ALLOWED
            .iter()
            .map(|(s, f)| (s.to_string(), f.to_string()))
            .collect();
        assert_eq!(
            shadows, allowed,
            "reserved-key shadows changed; extend the allowlist only for intentional event-owned values"
        );
    }

    use super::*;

    #[test]
    fn clone_ended_is_content_free_and_omits_absent_fields() {
        assert_eq!(CloneEnded::NAME, "clone_ended");
        assert_eq!(
            serde_json::to_value(CloneEnded {
                requested_history: CloneHistoryMode::Shallow,
                effective_history: Some(CloneHistoryMode::Shallow),
                duration_ms: 42,
                outcome: CloneOutcome::Success,
                failure_stage: None,
                source_mode: Some(CloneSourceMode::Local),
                transport: Some(CloneTransport::Fuse),
                requested_strategy: Some(CloneStrategy::Grove),
                resolved_strategy: Some(CloneStrategy::GroveFuse),
                fallback_reason: None,
                terminal_phase: Some(ClonePhase::Committed),
                cancellation_disposition: None,
                daemon_capability_class: Some(CloneDaemonCapabilityClass::Current),
            })
            .unwrap(),
            serde_json::json!({
                "requested_history": "shallow",
                "effective_history": "shallow",
                "duration_ms": 42,
                "outcome": "success",
                "source_mode": "local",
                "transport": "fuse",
                "requested_strategy": "grove",
                "resolved_strategy": "grove-fuse",
                "terminal_phase": "committed",
                "daemon_capability_class": "current",
            })
        );
        let failed = serde_json::to_value(CloneEnded {
            requested_history: CloneHistoryMode::Shallow,
            effective_history: None,
            duration_ms: 7,
            outcome: CloneOutcome::Failed,
            failure_stage: Some(CloneFailureStage::Preflight),
            source_mode: None,
            transport: None,
            requested_strategy: None,
            resolved_strategy: None,
            fallback_reason: Some(CloneFallbackReason::FuseUnavailable),
            terminal_phase: None,
            cancellation_disposition: None,
            daemon_capability_class: None,
        })
        .unwrap();
        assert_eq!(
            failed.get("failure_stage").and_then(|v| v.as_str()),
            Some("preflight")
        );
        assert_eq!(
            failed.get("fallback_reason").and_then(|v| v.as_str()),
            Some("fuse_unavailable")
        );
        assert!(failed.get("effective_history").is_none());
        assert!(failed.get("source_mode").is_none());
        assert!(failed.get("transport").is_none());
        assert!(failed.get("requested_strategy").is_none());
        assert!(failed.get("resolved_strategy").is_none());
        assert!(failed.get("terminal_phase").is_none());
        assert!(failed.get("cancellation_disposition").is_none());
        assert!(failed.get("daemon_capability_class").is_none());
        let text = failed.to_string();
        assert!(!text.contains("http"), "{text}");
        assert!(!text.contains("path"), "{text}");
        assert!(!text.contains("url"), "{text}");
        assert!(!text.contains("repo"), "{text}");
        assert!(!text.contains("/dev/fuse"), "{text}");
        let cancelled = serde_json::to_value(CloneEnded {
            requested_history: CloneHistoryMode::Shallow,
            effective_history: None,
            duration_ms: 3,
            outcome: CloneOutcome::Cancelled,
            failure_stage: None,
            source_mode: None,
            transport: None,
            requested_strategy: Some(CloneStrategy::Grove),
            resolved_strategy: None,
            fallback_reason: None,
            terminal_phase: Some(ClonePhase::Cancelled),
            cancellation_disposition: Some(CloneCancellationDisposition::ClientCancelled),
            daemon_capability_class: None,
        })
        .unwrap();
        assert_eq!(
            cancelled.get("outcome").and_then(|v| v.as_str()),
            Some("cancelled")
        );
        assert_eq!(
            cancelled
                .get("cancellation_disposition")
                .and_then(|v| v.as_str()),
            Some("client_cancelled")
        );
        assert_eq!(
            cancelled.get("terminal_phase").and_then(|v| v.as_str()),
            Some("cancelled")
        );
        assert!(cancelled.get("failure_stage").is_none());
        assert_eq!(CloneStrategy::from_strategy_str("nfs"), None);
        assert_eq!(ClonePhase::from_phase_str("not-a-phase"), None);
    }

    #[test]
    fn worktree_ended_is_content_free_and_omits_absent_fields() {
        assert_eq!(WorktreeEnded::NAME, "worktree_ended");
        assert_eq!(
            serde_json::to_value(WorktreeEnded {
                lifecycle: WorktreeLifecycle::Create,
                duration_ms: 42,
                outcome: CloneOutcome::Success,
                transport: Some(CloneTransport::Fuse),
                requested_strategy: Some(CloneStrategy::Grove),
                resolved_strategy: Some(CloneStrategy::GroveFuse),
                fallback_reason: None,
                cancellation_disposition: None,
                daemon_capability_class: Some(CloneDaemonCapabilityClass::Current),
            })
            .unwrap(),
            serde_json::json!({
                "lifecycle": "create",
                "duration_ms": 42,
                "outcome": "success",
                "transport": "fuse",
                "requested_strategy": "grove",
                "resolved_strategy": "grove-fuse",
                "daemon_capability_class": "current",
            })
        );
        let failed = serde_json::to_value(WorktreeEnded {
            lifecycle: WorktreeLifecycle::Fork,
            duration_ms: 7,
            outcome: CloneOutcome::Failed,
            transport: None,
            requested_strategy: Some(CloneStrategy::Grove),
            resolved_strategy: Some(CloneStrategy::Copy),
            fallback_reason: Some(CloneFallbackReason::FuseUnavailable),
            cancellation_disposition: None,
            daemon_capability_class: None,
        })
        .unwrap();
        assert_eq!(
            failed.get("lifecycle").and_then(|v| v.as_str()),
            Some("fork")
        );
        assert_eq!(
            failed.get("fallback_reason").and_then(|v| v.as_str()),
            Some("fuse_unavailable")
        );
        assert!(failed.get("transport").is_none());
        assert!(failed.get("cancellation_disposition").is_none());
        assert!(failed.get("daemon_capability_class").is_none());
        assert!(failed.get("source_mode").is_none());
        let text = failed.to_string();
        assert!(!text.contains("http"), "{text}");
        assert!(!text.contains("path"), "{text}");
        assert!(!text.contains("url"), "{text}");
        assert!(!text.contains("repo"), "{text}");
        assert!(!text.contains("/dev/fuse"), "{text}");
        let cancelled = serde_json::to_value(WorktreeEnded {
            lifecycle: WorktreeLifecycle::Fork,
            duration_ms: 3,
            outcome: CloneOutcome::Cancelled,
            transport: None,
            requested_strategy: Some(CloneStrategy::Grove),
            resolved_strategy: None,
            fallback_reason: None,
            cancellation_disposition: Some(CloneCancellationDisposition::ClientCancelled),
            daemon_capability_class: None,
        })
        .unwrap();
        assert_eq!(
            cancelled.get("outcome").and_then(|v| v.as_str()),
            Some("cancelled")
        );
        assert_eq!(
            cancelled
                .get("cancellation_disposition")
                .and_then(|v| v.as_str()),
            Some("client_cancelled")
        );
        assert_eq!(
            cancelled.get("lifecycle").and_then(|v| v.as_str()),
            Some("fork")
        );
        assert!(cancelled.get("fallback_reason").is_none());
    }

    fn redirect_event_samples() -> Vec<RedirectEvent> {
        vec![
            RedirectEvent::Applied(RedirectApplied {
                transport: RedirectTransport::Fuse,
                mechanism: RedirectMechanismKind::Bind,
                kind: RedirectTypeKind::Bind,
                source: RedirectSourceKind::Auto,
                trigger: RedirectTriggerKind::Attach,
                apply_ms: 12,
                replication: ReplicationKind::None,
                fallback_reason: None,
            }),
            RedirectEvent::Applied(RedirectApplied {
                transport: RedirectTransport::Nfs,
                mechanism: RedirectMechanismKind::Symlink,
                kind: RedirectTypeKind::Bind,
                source: RedirectSourceKind::User,
                trigger: RedirectTriggerKind::Ipc,
                apply_ms: 40,
                replication: ReplicationKind::Clonefile,
                fallback_reason: Some(RedirectFallbackReason::ImageCap),
            }),
            RedirectEvent::Applied(RedirectApplied {
                transport: RedirectTransport::Nfs,
                mechanism: RedirectMechanismKind::Symlink,
                kind: RedirectTypeKind::Bind,
                source: RedirectSourceKind::Repo,
                trigger: RedirectTriggerKind::IndexPublish,
                apply_ms: 7,
                replication: ReplicationKind::Copy,
                fallback_reason: Some(RedirectFallbackReason::ImageAttachFailed),
            }),
            RedirectEvent::FixupFailed(RedirectFixupFailed {
                transport: RedirectTransport::Projfs,
                mechanism: RedirectMechanismKind::Junction,
                kind: RedirectTypeKind::Symlink,
                source: RedirectSourceKind::Auto,
                trigger: RedirectTriggerKind::KillSwitch,
                initial_state: RedirectStateKind::Ok,
                reason: RedirectFailureReason::SharingViolation,
                fixup_ms: 3,
            }),
            RedirectEvent::FixupFailed(RedirectFixupFailed {
                transport: RedirectTransport::Fuse,
                mechanism: RedirectMechanismKind::Bind,
                kind: RedirectTypeKind::Bind,
                source: RedirectSourceKind::User,
                trigger: RedirectTriggerKind::PurgeContinue,
                initial_state: RedirectStateKind::Conflict,
                reason: RedirectFailureReason::PurgeFailed,
                fixup_ms: 900,
            }),
            RedirectEvent::FixupFailed(RedirectFixupFailed {
                transport: RedirectTransport::Nfs,
                mechanism: RedirectMechanismKind::Image,
                kind: RedirectTypeKind::Bind,
                source: RedirectSourceKind::Repo,
                trigger: RedirectTriggerKind::DestTreeChanged,
                initial_state: RedirectStateKind::NotMounted,
                reason: RedirectFailureReason::ImageTxnPending,
                fixup_ms: 1,
            }),
            RedirectEvent::FixupFailed(RedirectFixupFailed {
                transport: RedirectTransport::Nfs,
                mechanism: RedirectMechanismKind::Image,
                kind: RedirectTypeKind::Bind,
                source: RedirectSourceKind::Auto,
                trigger: RedirectTriggerKind::Attach,
                initial_state: RedirectStateKind::Conflict,
                reason: RedirectFailureReason::ImageScanOverflow,
                fixup_ms: 2500,
            }),
            RedirectEvent::FixupFailed(RedirectFixupFailed {
                transport: RedirectTransport::Fuse,
                mechanism: RedirectMechanismKind::Bind,
                kind: RedirectTypeKind::Bind,
                source: RedirectSourceKind::Auto,
                trigger: RedirectTriggerKind::Attach,
                initial_state: RedirectStateKind::UnknownMount,
                reason: RedirectFailureReason::UnattributedMount,
                fixup_ms: 4,
            }),
            RedirectEvent::FixupFailed(RedirectFixupFailed {
                transport: RedirectTransport::Fuse,
                mechanism: RedirectMechanismKind::Bind,
                kind: RedirectTypeKind::Bind,
                source: RedirectSourceKind::User,
                trigger: RedirectTriggerKind::Ipc,
                initial_state: RedirectStateKind::Busy,
                reason: RedirectFailureReason::InFlight,
                fixup_ms: 0,
            }),
            RedirectEvent::Demoted(RedirectDemoted {
                transport: RedirectTransport::Fuse,
                mechanism: RedirectMechanismKind::Bind,
                reason: RedirectDemoteReason::KillSwitch,
                demote_ms: 5,
            }),
            RedirectEvent::Overwrite(RedirectOverwrite {
                transport: RedirectTransport::Nfs,
                mechanism: RedirectMechanismKind::Image,
                disposition: OverwriteDisposition::Replicated,
                replication: ReplicationKind::Move,
                entries_moved: 1200,
                replicate_ms: 250,
            }),
            RedirectEvent::LimitHit(RedirectLimitHit {
                limit_kind: RedirectLimitKind::UserEntries,
                limit: 64,
                observed: 65,
                disposition: LimitDisposition::Rejected,
            }),
            RedirectEvent::LimitHit(RedirectLimitHit {
                limit_kind: RedirectLimitKind::ImagesPerMount,
                limit: 8,
                observed: 9,
                disposition: LimitDisposition::FallbackSymlink,
            }),
            RedirectEvent::LimitHit(RedirectLimitHit {
                limit_kind: RedirectLimitKind::ImageScanEntries,
                limit: 100000,
                observed: 100001,
                disposition: LimitDisposition::Refused,
            }),
            RedirectEvent::LimitHit(RedirectLimitHit {
                limit_kind: RedirectLimitKind::PurgeBudget,
                limit: 5000,
                observed: 5000,
                disposition: LimitDisposition::Continued,
            }),
        ]
    }

    /// Byte-identical to grove's `redirect_events_wire_fixture` array, because
    /// the daemon mirrors these types without linking this crate.
    const REDIRECT_EVENT_WIRE_FIXTURE: &[&str] = &[
        r#"{"event":"redirect_applied","transport":"fuse","mechanism":"bind","kind":"bind","source":"auto","trigger":"attach","apply_ms":12,"replication":"none"}"#,
        r#"{"event":"redirect_applied","transport":"nfs","mechanism":"symlink","kind":"bind","source":"user","trigger":"ipc","apply_ms":40,"replication":"clonefile","fallback_reason":"image_cap"}"#,
        r#"{"event":"redirect_applied","transport":"nfs","mechanism":"symlink","kind":"bind","source":"repo","trigger":"index_publish","apply_ms":7,"replication":"copy","fallback_reason":"image_attach_failed"}"#,
        r#"{"event":"redirect_fixup_failed","transport":"projfs","mechanism":"junction","kind":"symlink","source":"auto","trigger":"kill_switch","initial_state":"ok","reason":"sharing_violation","fixup_ms":3}"#,
        r#"{"event":"redirect_fixup_failed","transport":"fuse","mechanism":"bind","kind":"bind","source":"user","trigger":"purge_continue","initial_state":"conflict","reason":"purge_failed","fixup_ms":900}"#,
        r#"{"event":"redirect_fixup_failed","transport":"nfs","mechanism":"image","kind":"bind","source":"repo","trigger":"dest_tree_changed","initial_state":"not_mounted","reason":"image_txn_pending","fixup_ms":1}"#,
        r#"{"event":"redirect_fixup_failed","transport":"nfs","mechanism":"image","kind":"bind","source":"auto","trigger":"attach","initial_state":"conflict","reason":"image_scan_overflow","fixup_ms":2500}"#,
        r#"{"event":"redirect_fixup_failed","transport":"fuse","mechanism":"bind","kind":"bind","source":"auto","trigger":"attach","initial_state":"unknown_mount","reason":"unattributed_mount","fixup_ms":4}"#,
        r#"{"event":"redirect_fixup_failed","transport":"fuse","mechanism":"bind","kind":"bind","source":"user","trigger":"ipc","initial_state":"busy","reason":"in_flight","fixup_ms":0}"#,
        r#"{"event":"redirect_demoted","transport":"fuse","mechanism":"bind","reason":"kill_switch","demote_ms":5}"#,
        r#"{"event":"redirect_overwrite","transport":"nfs","mechanism":"image","disposition":"replicated","replication":"move","entries_moved":1200,"replicate_ms":250}"#,
        r#"{"event":"redirect_limit_hit","limit_kind":"user_entries","limit":64,"observed":65,"disposition":"rejected"}"#,
        r#"{"event":"redirect_limit_hit","limit_kind":"images_per_mount","limit":8,"observed":9,"disposition":"fallback_symlink"}"#,
        r#"{"event":"redirect_limit_hit","limit_kind":"image_scan_entries","limit":100000,"observed":100001,"disposition":"refused"}"#,
        r#"{"event":"redirect_limit_hit","limit_kind":"purge_budget","limit":5000,"observed":5000,"disposition":"continued"}"#,
    ];

    /// Every variant of every wire enum in declaration order, byte-identical to
    /// grove's `redirect_enum_words_are_exhaustive_fixture` array.
    const REDIRECT_ENUM_WORDS: &[(&str, &str)] = &[
        ("RedirectTransport", "fuse,nfs,projfs"),
        ("RedirectMechanismKind", "bind,symlink,image,junction"),
        ("RedirectTypeKind", "bind,symlink"),
        ("RedirectSourceKind", "user,repo,auto"),
        (
            "RedirectTriggerKind",
            "attach,kill_switch,index_publish,dest_tree_changed,purge_continue,ipc",
        ),
        (
            "RedirectStateKind",
            "ok,ok_fallback,unknown_mount,not_mounted,symlink_missing,symlink_incorrect,conflict,busy,capability_unavailable",
        ),
        ("RedirectFallbackReason", "image_attach_failed,image_cap"),
        (
            "RedirectFailureReason",
            "ebusy,sharing_violation,demote_budget,in_flight,live_dir,foreign_object,foreign_link,foreign_mount,occupied,overlap,parent_is_link,not_ignored,index_tracked,unattributed_mount,conversion_failed,image_txn_pending,image_scan_overflow,dest_claimed,eperm,attach_timeout,attach_failed,create_failed,remount_failed,copy_failed,verify_failed,repo_file_invalid,purge_failed,identity_refused,cancelled,io",
        ),
        ("ReplicationKind", "none,clonefile,copy,move"),
        ("OverwriteDisposition", "replicated,refused,forced"),
        (
            "RedirectDemoteReason",
            "index_tracked,ipc,shutdown,cleanup,stuck_lazy,convert,fixup,del,kill_switch",
        ),
        (
            "RedirectLimitKind",
            "auto_candidates,repo_file_entries,repo_file_bytes,user_entries,images_per_mount,purge_budget,image_scan_entries",
        ),
        (
            "LimitDisposition",
            "truncated,rejected,fallback_symlink,continued,refused",
        ),
    ];

    /// Serializes every variant and checks each word parses back to it.
    fn enum_words<T>(all: &[T]) -> String
    where
        T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        all.iter()
            .map(|v| {
                let word = serde_json::to_value(v).unwrap();
                assert_eq!(*v, serde_json::from_value::<T>(word.clone()).unwrap());
                word.as_str().unwrap().to_owned()
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    #[test]
    fn redirect_enum_words_are_exhaustive_fixture() {
        use strum::VariantArray;
        macro_rules! words {
            ($($t:ty),* $(,)?) => { [$((stringify!($t), enum_words(<$t>::VARIANTS))),*] };
        }
        let actual = words![
            RedirectTransport,
            RedirectMechanismKind,
            RedirectTypeKind,
            RedirectSourceKind,
            RedirectTriggerKind,
            RedirectStateKind,
            RedirectFallbackReason,
            RedirectFailureReason,
            ReplicationKind,
            OverwriteDisposition,
            RedirectDemoteReason,
            RedirectLimitKind,
            LimitDisposition,
        ];
        let expected: Vec<(&str, String)> = REDIRECT_ENUM_WORDS
            .iter()
            .map(|(name, words)| (*name, (*words).to_owned()))
            .collect();
        assert_eq!(expected, actual.to_vec());
    }

    #[test]
    fn redirect_events_are_content_free_fixture() {
        assert_eq!(RedirectApplied::NAME, "redirect_applied");
        assert_eq!(RedirectFixupFailed::NAME, "redirect_fixup_failed");
        assert_eq!(RedirectDemoted::NAME, "redirect_demoted");
        assert_eq!(RedirectOverwrite::NAME, "redirect_overwrite");
        assert_eq!(RedirectLimitHit::NAME, "redirect_limit_hit");
        let closed_words: std::collections::BTreeSet<&str> = REDIRECT_ENUM_WORDS
            .iter()
            .flat_map(|(_, words)| words.split(','))
            .collect();
        for event in redirect_event_samples() {
            let value = serde_json::to_value(&event).unwrap();
            let object = value.as_object().unwrap();
            for (key, field) in object {
                for banned in ["path", "url", "name", "dest", "/"] {
                    assert!(!key.contains(banned), "{key} in {value}");
                }
                match field {
                    serde_json::Value::String(word) => assert!(
                        key == "event" || closed_words.contains(word.as_str()),
                        "{key}={word} is not a closed enum word in {value}"
                    ),
                    serde_json::Value::Number(n) => assert!(n.is_u64(), "{key} in {value}"),
                    other => panic!("{key}={other} is neither an enum word nor a count"),
                }
            }
        }
        let applied = serde_json::to_value(RedirectApplied {
            transport: RedirectTransport::Fuse,
            mechanism: RedirectMechanismKind::Bind,
            kind: RedirectTypeKind::Bind,
            source: RedirectSourceKind::Auto,
            trigger: RedirectTriggerKind::Attach,
            apply_ms: 12,
            replication: ReplicationKind::None,
            fallback_reason: None,
        })
        .unwrap();
        assert!(applied.get("fallback_reason").is_none(), "{applied}");
        assert!(applied.get("event").is_none(), "{applied}");
    }

    #[test]
    fn redirect_events_wire_fixture() {
        let samples = redirect_event_samples();
        assert_eq!(REDIRECT_EVENT_WIRE_FIXTURE.len(), samples.len());
        for (literal, event) in REDIRECT_EVENT_WIRE_FIXTURE.iter().zip(&samples) {
            assert_eq!(*literal, serde_json::to_string(event).unwrap());
            let parsed: RedirectEvent = serde_json::from_str(literal).unwrap();
            assert_eq!(*event, parsed);
            assert_eq!(*literal, serde_json::to_string(&parsed).unwrap());
        }
        for event in &samples {
            let name = match event {
                RedirectEvent::Applied(_) => RedirectApplied::NAME,
                RedirectEvent::FixupFailed(_) => RedirectFixupFailed::NAME,
                RedirectEvent::Demoted(_) => RedirectDemoted::NAME,
                RedirectEvent::Overwrite(_) => RedirectOverwrite::NAME,
                RedirectEvent::LimitHit(_) => RedirectLimitHit::NAME,
            };
            let value = serde_json::to_value(event).unwrap();
            assert_eq!(value.get("event").and_then(|v| v.as_str()), Some(name));
        }
    }

    #[test]
    fn process_resource_usage_omits_allocated_bytes_when_unavailable() {
        assert_eq!(
            serde_json::to_value(ProcessResourceUsage {
                trigger: ResourceReportTrigger::Periodic,
                rss_bytes: None,
                peak_rss_bytes: None,
                footprint_bytes: None,
                allocated_bytes: Some(4_096),
                threads: None,
                open_files: None,
                resident_sessions: 2,
                session_threads: 3,
                idle: false,
            })
            .unwrap(),
            serde_json::json!({
                "trigger": "periodic",
                "allocated_bytes": 4_096,
                "resident_sessions": 2,
                "session_threads": 3,
                "idle": false,
            })
        );
        assert_eq!(
            serde_json::to_value(ProcessResourceUsage {
                trigger: ResourceReportTrigger::Periodic,
                rss_bytes: None,
                peak_rss_bytes: None,
                footprint_bytes: None,
                allocated_bytes: None,
                threads: None,
                open_files: None,
                resident_sessions: 2,
                session_threads: 3,
                idle: false,
            })
            .unwrap(),
            serde_json::json!({
                "trigger": "periodic",
                "resident_sessions": 2,
                "session_threads": 3,
                "idle": false,
            })
        );
    }

    #[test]
    fn tool_call_completed_omits_tool_result_size_bytes_when_absent() {
        assert_eq!(
            serde_json::to_value(ToolCallCompleted {
                tool_name: "bash".into(),
                outcome: distill_session_events::types::ToolOutcome::Success,
                hook_rewrote: false,
                duration_ms: 7,
                tool_result_size_bytes: Some(2_048),
                model_id: "grok".into(),
                file_path: None,
                parameters: None,
                tool_use_id: None,
                tool_output: None,
                error_message: None,
            })
            .unwrap(),
            serde_json::json!({
                "tool_name": "bash",
                "outcome": "success",
                "hook_rewrote": false,
                "duration_ms": 7,
                "tool_result_size_bytes": 2_048,
            })
        );
        assert_eq!(
            serde_json::to_value(ToolCallCompleted {
                tool_name: "bash".into(),
                outcome: distill_session_events::types::ToolOutcome::Success,
                hook_rewrote: false,
                duration_ms: 7,
                tool_result_size_bytes: None,
                model_id: "grok".into(),
                file_path: None,
                parameters: None,
                tool_use_id: None,
                tool_output: None,
                error_message: None,
            })
            .unwrap(),
            serde_json::json!({
                "tool_name": "bash",
                "outcome": "success",
                "hook_rewrote": false,
                "duration_ms": 7,
            })
        );
    }

    #[test]
    fn auth_lock_wait_event_carries_wait_and_budget() {
        assert_eq!(
            serde_json::to_value(AuthLockWait {
                wait_ms: 4321,
                budget_ms: 25_000,
            })
            .unwrap(),
            serde_json::json!({ "wait_ms": 4321, "budget_ms": 25_000 })
        );
    }

    #[test]
    fn auth_lock_timeout_event_omits_an_unknown_holder_state() {
        assert_eq!(
            serde_json::to_value(AuthLockTimeout {
                budget_ms: 25_000,
                holder_state: Some("stuck_live"),
            })
            .unwrap(),
            serde_json::json!({ "budget_ms": 25_000, "holder_state": "stuck_live" })
        );
        assert_eq!(
            serde_json::to_value(AuthLockTimeout {
                budget_ms: 10_000,
                holder_state: None,
            })
            .unwrap(),
            serde_json::json!({ "budget_ms": 10_000 })
        );
    }

    #[test]
    fn auth_lock_replaced_event_omits_unknown_holder_fields() {
        assert_eq!(
            serde_json::to_value(AuthLockReplacedOutFromUnder {
                holder_pid: Some(42),
                holder_state: Some("alive"),
                holder_age_secs: None,
            })
            .unwrap(),
            serde_json::json!({ "holder_pid": 42, "holder_state": "alive" })
        );
    }

    fn terminal_telemetry_fixture() -> TerminalTelemetry {
        TerminalTelemetry {
            brand: "Unknown".into(),
            multiplexer: "none".into(),
            is_ssh: true,
            is_byobu: false,
            term_var: "xterm-256color".into(),
            tmux_version: "".into(),
            xtversion: "".into(),
            term_version: "".into(),
            term_version_source: "none".into(),
            kitty_event_types_withheld: false,
            host_os: "linux".into(),
            display_server: "unknown".into(),
            modifier_cmd_fate: "unknown".into(),
            modifier_opt_fate: "unknown".into(),
            enter_modifier_fate: "unknown".into(),
            hyperlink_osc8: "unknown".into(),
            hyperlink_skip_reason: "none".into(),
            clipboard_route: "native+osc52".into(),
            clipboard_native_tool: "arboard".into(),
            clipboard_data_control: "n/a".into(),
        }
    }

    #[test]
    fn memory_retrieval_mode_serializes_as_closed_snake_case_values() {
        let modes = [
            MemoryRetrievalMode::Disabled,
            MemoryRetrievalMode::FtsOnly,
            MemoryRetrievalMode::Hybrid,
        ];
        assert_eq!(
            modes.map(|mode| serde_json::to_value(mode).unwrap()),
            ["disabled", "fts_only", "hybrid"]
        );
    }

    /// Both closed sets are what dashboards split on; a renamed variant or a lost `rename_all` fails here.
    #[test]
    fn clipboard_paste_enums_serialize_as_closed_snake_case_values() {
        let paths = [
            ClipboardReadPath::Native,
            ClipboardReadPath::Osascript,
            ClipboardReadPath::Arboard,
            ClipboardReadPath::LinuxCli,
        ];
        assert_eq!(
            ["native", "osascript", "arboard", "linux_cli"],
            paths.map(|path| serde_json::to_value(path).unwrap())
        );
        let reasons = [
            ClipboardProbeDropReason::PasteboardChangedBeforeRead,
            ClipboardProbeDropReason::PasteboardChangedAfterRead,
            ClipboardProbeDropReason::BracketedPayloadMismatch,
            ClipboardProbeDropReason::ReadFailed,
            ClipboardProbeDropReason::Timeout,
            ClipboardProbeDropReason::PersistFailed,
            ClipboardProbeDropReason::Panicked,
        ];
        assert_eq!(
            [
                "pasteboard_changed_before_read",
                "pasteboard_changed_after_read",
                "bracketed_payload_mismatch",
                "read_failed",
                "timeout",
                "persist_failed",
                "panicked",
            ],
            reasons.map(|reason| serde_json::to_value(reason).unwrap())
        );
    }

    /// `read_path` names the backend on a completed read and is absent on an error; a drop carries the raster's hash.
    #[test]
    fn clipboard_paste_events_shape() {
        let paste = |outcome: &str, read_path, hash: &str| {
            serde_json::to_value(ClipboardImagePaste {
                terminal: terminal_telemetry_fixture(),
                probe: "attachments".into(),
                outcome: outcome.into(),
                read_path,
                image_mime: String::new(),
                image_hash: hash.into(),
                image_bytes: 0,
                duration_ms: 3,
            })
            .unwrap()
        };
        let fallback = paste(
            "image",
            Some(ClipboardReadPath::Osascript),
            &"ab".repeat(32),
        );
        assert_eq!(
            fallback.get("read_path"),
            Some(&serde_json::json!("osascript"))
        );
        assert_eq!(
            fallback.get("image_hash"),
            Some(&serde_json::json!("ab".repeat(32)))
        );
        let error = paste("error", None, "");
        assert!(error.get("read_path").is_none());

        assert_eq!(
            ClipboardPasteProbeDropped::NAME,
            "clipboard_paste_probe_dropped"
        );
        let dropped = serde_json::to_value(ClipboardPasteProbeDropped {
            terminal: terminal_telemetry_fixture(),
            reason: ClipboardProbeDropReason::PasteboardChangedAfterRead,
            image_hash: "cd".repeat(32),
            duration_ms: 530,
        })
        .unwrap();
        assert_eq!(
            dropped.get("reason"),
            Some(&serde_json::json!("pasteboard_changed_after_read"))
        );
        assert_eq!(
            dropped.get("image_hash"),
            Some(&serde_json::json!("cd".repeat(32)))
        );
    }

    #[test]
    fn clipboard_copy_serialization_preserves_boolean_and_adds_delivery_evidence() {
        for delivery in ["confirmed", "unverified", "failed"] {
            let value = serde_json::to_value(ClipboardCopy {
                terminal: terminal_telemetry_fixture(),
                source: "copy_text",
                text_len: 12,
                route_native: true,
                route_tmux: false,
                route_osc52: true,
                route_label: "native+osc52".into(),
                cli_tools_tried: String::new(),
                cli_ok_tools: String::new(),
                cli_ok: false,
                arboard_ok: false,
                data_control: false,
                tmux_ok: false,
                osc52_ok: true,
                delivery,
                osc52_sink: false,
                container_no_display: false,
                reported_success: delivery != "failed",
                toast_kind: "unverified_osc_remote",
                duration_ms: 1,
            })
            .unwrap();
            assert_eq!(value.get("delivery"), Some(&serde_json::json!(delivery)));
            assert_eq!(
                value.get("reported_success"),
                Some(&serde_json::Value::Bool(delivery != "failed"))
            );
            assert_eq!(value.get("osc52_sink"), Some(&serde_json::json!(false)));
            assert_eq!(
                value.get("container_no_display"),
                Some(&serde_json::json!(false))
            );
        }
    }

    #[test]
    fn manual_auth_name_and_shape() {
        assert_eq!(ManualAuth::NAME, "manual_auth");

        let with_principal = serde_json::to_value(ManualAuth {
            reason: ManualAuthReason::RefreshTokenRejected,
            trigger: ManualAuthSurface::Turn,
            token_kind: AuthTokenKind::OidcSession,
            principal: Some("user-1".into()),
        })
        .unwrap();
        assert_eq!(
            with_principal,
            serde_json::json!({
                "reason": "refresh_token_rejected",
                "trigger": "turn",
                "token_kind": "oidc_session",
                "principal": "user-1",
            })
        );

        // `principal` is omitted (not null) when unknown
        // `LegacySession` is a reachable fixture (API-key sessions never emit this event)
        let no_principal = serde_json::to_value(ManualAuth {
            reason: ManualAuthReason::NoRefreshAuthority,
            trigger: ManualAuthSurface::Relay,
            token_kind: AuthTokenKind::LegacySession,
            principal: None,
        })
        .unwrap();
        assert!(!no_principal.as_object().unwrap().contains_key("principal"));
    }

    #[test]
    fn plugin_cta_event_names() {
        assert_eq!(PluginCtaImpression::NAME, "plugin_cta_impression");
        assert_eq!(PluginCtaConnectClicked::NAME, "plugin_cta_connect_clicked");
        assert_eq!(PluginCtaDismissed::NAME, "plugin_cta_dismissed");
        assert_eq!(PluginCtaInstalled::NAME, "plugin_cta_installed");
    }

    #[test]
    fn announcement_cta_event_names() {
        assert_eq!(AnnouncementCtaShown::NAME, "announcement_cta_shown");
        assert_eq!(AnnouncementCtaClicked::NAME, "announcement_cta_clicked");
    }

    #[test]
    fn shortcut_used_name_and_shape() {
        assert_eq!(ShortcutUsed::NAME, "shortcut_used");
        let value = serde_json::to_value(ShortcutUsed {
            key: "Ctrl+L".into(),
            action: "interject_prompt".into(),
            context: "prompt_focused".into(),
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "key": "Ctrl+L",
                "action": "interject_prompt",
                "context": "prompt_focused",
            })
        );
    }

    #[test]
    fn coding_data_consent_selected_name_and_shape() {
        assert_eq!(
            CodingDataConsentSelected::NAME,
            "coding_data_consent_selected"
        );
        let event = serde_json::to_value(CodingDataConsentSelected {
            source: CodingDataConsentSource::Settings,
            choice: CodingDataConsentChoice::OptIn,
            previous_choice: CodingDataConsentChoice::OptIn,
            changed: false,
        })
        .unwrap();
        assert_eq!(
            event,
            serde_json::json!({
                "source": "settings",
                "choice": "opt_in",
                "previous_choice": "opt_in",
                "changed": false,
            })
        );
    }

    /// Serde renames the payload field, strum renders the external label: two snake_case implementations, so pin that they agree on every variant.
    #[test]
    fn skill_trigger_serializes_the_same_string_strum_yields() {
        for trigger in [
            SkillTrigger::SlashCommand,
            SkillTrigger::SkillMdRead,
            SkillTrigger::SkillTool,
        ] {
            let serde = serde_json::to_value(SkillDispatched {
                skill_name: "pdf".into(),
                plugin_source: None,
                trigger,
                skill_source: Some("bundled".into()),
            })
            .unwrap();
            assert_eq!(
                serde,
                serde_json::json!({
                    "skill_name": "pdf",
                    "trigger": <&'static str>::from(trigger),
                    "skill_source": "bundled",
                })
            );
        }
        let omitted = serde_json::to_value(SkillDispatched {
            skill_name: "pdf".into(),
            plugin_source: None,
            trigger: SkillTrigger::SlashCommand,
            skill_source: None,
        })
        .unwrap();
        assert_eq!(
            omitted,
            serde_json::json!({ "skill_name": "pdf", "trigger": "slash_command" })
        );
    }

    #[test]
    fn compaction_retry_degraded_name_and_shape() {
        assert_eq!(CompactionRetryDegraded::NAME, "compaction_retry_degraded");

        let degenerate = serde_json::to_value(CompactionRetryDegraded {
            trigger: CompactionTrigger::Auto,
            reason: "degenerate_summary",
            from_stage: None,
            to_stage: None,
            summary_chars: Some(130),
            attempt: 1,
            context_window: 128_000,
            compaction_id: "cid-1".into(),
        })
        .unwrap();
        assert_eq!(
            degenerate,
            serde_json::json!({
                "trigger": "auto",
                "reason": "degenerate_summary",
                "summary_chars": 130,
                "attempt": 1,
                "context_window": 128_000,
                "compaction_id": "cid-1",
            })
        );

        let overflow = serde_json::to_value(CompactionRetryDegraded {
            trigger: CompactionTrigger::Manual,
            reason: "input_overflow",
            from_stage: Some("verbatim"),
            to_stage: Some("verbatim_fitted"),
            summary_chars: None,
            attempt: 2,
            context_window: 128_000,
            compaction_id: "cid-2".into(),
        })
        .unwrap();
        assert_eq!(
            overflow,
            serde_json::json!({
                "trigger": "manual",
                "reason": "input_overflow",
                "from_stage": "verbatim",
                "to_stage": "verbatim_fitted",
                "attempt": 2,
                "context_window": 128_000,
                "compaction_id": "cid-2",
            })
        );
    }

    #[test]
    fn compaction_triggered_name_and_shape() {
        assert_eq!(CompactionTriggered::NAME, "compaction_triggered");
        let event = serde_json::to_value(CompactionTriggered {
            trigger: CompactionTrigger::Auto,
            tokens_used: 100_000,
            context_window: 128_000,
            percentage: 78,
            model_id: "grok-4".into(),
            user_context_provided: false,
            compaction_id: "cid-1".into(),
            compaction_mode: CompactionModeLabel::Segments,
            two_pass_enabled: true,
            is_subagent: false,
        })
        .unwrap();
        assert_eq!(
            event,
            serde_json::json!({
                "trigger": "auto",
                "tokens_used": 100_000,
                "context_window": 128_000,
                "percentage": 78,
                "model_id": "grok-4",
                "user_context_provided": false,
                "compaction_id": "cid-1",
                "compaction_mode": "segments",
                "two_pass_enabled": true,
                "is_subagent": false,
            })
        );

        let disarmed = serde_json::to_value(CompactionTriggered {
            trigger: CompactionTrigger::Manual,
            tokens_used: 10_000,
            context_window: 128_000,
            percentage: 8,
            model_id: "grok-4".into(),
            user_context_provided: false,
            compaction_id: "cid-2".into(),
            compaction_mode: CompactionModeLabel::Summary,
            two_pass_enabled: false,
            is_subagent: false,
        })
        .unwrap();
        assert_eq!(
            disarmed,
            serde_json::json!({
                "trigger": "manual",
                "tokens_used": 10_000,
                "context_window": 128_000,
                "percentage": 8,
                "model_id": "grok-4",
                "user_context_provided": false,
                "compaction_id": "cid-2",
                "compaction_mode": "summary",
                "two_pass_enabled": false,
                "is_subagent": false,
            })
        );
    }

    #[test]
    fn compaction_completed_name_and_shape() {
        assert_eq!(CompactionCompleted::NAME, "compaction_completed");
        let with_model = serde_json::to_value(CompactionCompleted {
            duration_ms: 63_000,
            tokens_before: 399_000,
            tokens_after: 15_000,
            model_id: Some("grok-4".into()),
            compaction_id: "cid-1".into(),
            compaction_mode: CompactionModeLabel::Summary,
            two_pass: TwoPassOutcome::TwoPass,
            segments_queued: 0,
            degenerate_retries: 1,
            input_overflow_retries: 2,
            is_subagent: false,
            model_wait_ms: None,
            pre_compaction_ms: None,
            post_compaction_ms: None,
        })
        .unwrap();
        assert_eq!(
            with_model,
            serde_json::json!({
                "duration_ms": 63_000,
                "tokens_before": 399_000,
                "tokens_after": 15_000,
                "model_id": "grok-4",
                "compaction_id": "cid-1",
                "compaction_mode": "summary",
                "two_pass": "two_pass",
                "segments_queued": 0,
                "degenerate_retries": 1,
                "input_overflow_retries": 2,
                "is_subagent": false,
            })
        );

        let no_model = serde_json::to_value(CompactionCompleted {
            duration_ms: 1,
            tokens_before: 1,
            tokens_after: 1,
            model_id: None,
            compaction_id: "cid-2".into(),
            compaction_mode: CompactionModeLabel::Transcript,
            two_pass: TwoPassOutcome::Disabled,
            segments_queued: 0,
            degenerate_retries: 0,
            input_overflow_retries: 0,
            is_subagent: true,
            model_wait_ms: None,
            pre_compaction_ms: None,
            post_compaction_ms: None,
        })
        .unwrap();
        assert_eq!(
            no_model,
            serde_json::json!({
                "duration_ms": 1,
                "tokens_before": 1,
                "tokens_after": 1,
                "compaction_id": "cid-2",
                "compaction_mode": "transcript",
                "two_pass": "disabled",
                "segments_queued": 0,
                "degenerate_retries": 0,
                "input_overflow_retries": 0,
                "is_subagent": true,
            })
        );

        let single_pass = serde_json::to_value(CompactionCompleted {
            duration_ms: 2,
            tokens_before: 2,
            tokens_after: 2,
            model_id: None,
            compaction_id: "cid-3".into(),
            compaction_mode: CompactionModeLabel::Segments,
            two_pass: TwoPassOutcome::SinglePass,
            segments_queued: 1,
            degenerate_retries: 0,
            input_overflow_retries: 0,
            is_subagent: false,
            model_wait_ms: None,
            pre_compaction_ms: None,
            post_compaction_ms: None,
        })
        .unwrap();
        assert_eq!(
            single_pass,
            serde_json::json!({
                "duration_ms": 2,
                "tokens_before": 2,
                "tokens_after": 2,
                "compaction_id": "cid-3",
                "compaction_mode": "segments",
                "two_pass": "single_pass",
                "segments_queued": 1,
                "degenerate_retries": 0,
                "input_overflow_retries": 0,
                "is_subagent": false,
            })
        );
    }

    #[test]
    fn plugin_cta_impression_serializes_plugin_name() {
        let v = serde_json::to_value(PluginCtaImpression {
            plugin_name: "figma".into(),
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "plugin_name": "figma" }));
    }

    #[test]
    fn plugin_cta_connect_clicked_serializes_is_retry() {
        let fresh = serde_json::to_value(PluginCtaConnectClicked {
            plugin_name: "figma".into(),
            is_retry: false,
        })
        .unwrap();
        assert_eq!(
            fresh,
            serde_json::json!({ "plugin_name": "figma", "is_retry": false })
        );
        let retry = serde_json::to_value(PluginCtaConnectClicked {
            plugin_name: "figma".into(),
            is_retry: true,
        })
        .unwrap();
        assert_eq!(
            retry,
            serde_json::json!({ "plugin_name": "figma", "is_retry": true })
        );
    }

    #[test]
    fn plugin_cta_installed_omits_error_category_when_none() {
        let v = serde_json::to_value(PluginCtaInstalled {
            plugin_name: "figma".into(),
            success: true,
            error_category: None,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "plugin_name": "figma", "success": true })
        );
    }

    #[test]
    fn login_funnel_event_names() {
        assert_eq!(LoginPickerShown::NAME, "login_picker_shown");
        assert_eq!(LoginMethodChosen::NAME, "login_method_chosen");
        assert_eq!(LoginCompleted::NAME, "login_completed");
        assert_eq!(LoginFailed::NAME, "login_failed");
        assert_eq!(LoginAbandoned::NAME, "login_abandoned");
        assert_eq!(ApiKeySaveResult::NAME, "api_key_save_result");
    }

    #[test]
    fn login_completed_serializes_all_fields() {
        let v = serde_json::to_value(LoginCompleted {
            method: "xai".into(),
            mode: "device".into(),
            duration_ms: 1234,
            mid_session: false,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "method": "xai",
                "mode": "device",
                "duration_ms": 1234,
                "mid_session": false,
            })
        );
    }

    #[test]
    fn login_failed_serializes_kind_and_os_code() {
        let v = serde_json::to_value(LoginFailed {
            error_kind: LoginFailureKind::TransportInterrupted,
            os_error: Some(104),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({ "error_kind": "transport_interrupted", "os_error": 104 })
        );
    }

    #[test]
    fn login_failed_omits_absent_os_code() {
        let v = serde_json::to_value(LoginFailed {
            error_kind: LoginFailureKind::Decode,
            os_error: None,
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "error_kind": "decode" }));
    }

    #[test]
    fn api_key_save_result_omits_error_when_ok() {
        let ok = serde_json::to_value(ApiKeySaveResult {
            ok: true,
            error: None,
        })
        .unwrap();
        assert_eq!(ok, serde_json::json!({ "ok": true }));
        let err = serde_json::to_value(ApiKeySaveResult {
            ok: false,
            error: Some("failed to write key".into()),
        })
        .unwrap();
        assert_eq!(
            err,
            serde_json::json!({ "ok": false, "error": "failed to write key" })
        );
    }

    #[test]
    fn turn_completed_error_fields_omit_when_none_include_when_some() {
        fn tc(error_code: Option<String>, error_detail: Option<String>) -> TurnCompleted {
            TurnCompleted {
                outcome: Outcome::Completed,
                duration_ms: 5,
                tool_call_count: 0,
                model_id: "grok-4".into(),
                session_id: None,
                cancellation_category: None,
                error_category: None,
                error_code,
                error_detail,
                context_tokens: None,
                turn_tokens: None,
            }
        }
        let omitted = serde_json::to_value(tc(None, None)).unwrap();
        assert!(
            omitted.get("error_code").is_none(),
            "error_code must be omitted when None"
        );
        assert!(
            omitted.get("error_detail").is_none(),
            "error_detail must be omitted when None"
        );
        let included =
            serde_json::to_value(tc(Some("invalid_request".into()), Some("bad body".into())))
                .unwrap();
        assert_eq!(
            included.get("error_code").and_then(|v| v.as_str()),
            Some("invalid_request")
        );
        assert_eq!(
            included.get("error_detail").and_then(|v| v.as_str()),
            Some("bad body")
        );
    }

    #[test]
    fn turn_completed_context_tokens_omit_when_none_include_when_some() {
        fn tc(context_tokens: Option<u64>) -> TurnCompleted {
            TurnCompleted {
                outcome: Outcome::Completed,
                duration_ms: 1200,
                tool_call_count: 3,
                model_id: "grok-4.6".into(),
                session_id: None,
                cancellation_category: None,
                error_category: None,
                error_code: None,
                error_detail: None,
                context_tokens,
                turn_tokens: None,
            }
        }
        let included = serde_json::to_value(tc(Some(204_958))).unwrap();
        assert_eq!(
            included,
            serde_json::json!({
                "outcome": "completed",
                "duration_ms": 1200,
                "tool_call_count": 3,
                "model_id": "grok-4.6",
                "context_tokens": 204_958,
            })
        );
        let omitted = serde_json::to_value(tc(None)).unwrap();
        assert!(
            omitted.get("context_tokens").is_none(),
            "context_tokens must be omitted, not zero, when None: {omitted}"
        );
    }

    #[test]
    fn model_response_received_carries_per_call_context_tokens() {
        let v = serde_json::to_value(ModelResponseReceived {
            model_id: "grok-4.6".into(),
            duration_ms: 900,
            stop_reason: None,
            prompt_tokens: Some(26_886),
            completion_tokens: Some(52),
            reasoning_tokens: None,
            cached_prompt_tokens: None,
            cache_creation_tokens: None,
            context_tokens: Some(26_938),
            cost_usd_ticks: None,
        })
        .unwrap();
        assert_eq!(
            v.get("context_tokens").and_then(|c| c.as_u64()),
            Some(26_938)
        );
        assert_eq!(
            v.get("prompt_tokens").and_then(|c| c.as_u64()),
            Some(26_886)
        );
    }

    #[test]
    fn cli_update_event_name_and_serde() {
        assert_eq!(CliUpdate::NAME, "cli_update");
        let ok = serde_json::to_value(CliUpdate {
            outcome: CliUpdateOutcome::Success,
            trigger: CliUpdateTrigger::UserCommand,
            from_version: "0.2.118".into(),
            to_version: Some("0.2.120".into()),
            channel: CliUpdateChannel::Alpha,
            installer: CliUpdateInstaller::Internal,
            platform: "macos-x86_64".into(),
            rosetta: true,
            duration_ms: 12_000,
            error_kind: None,
        })
        .unwrap();
        assert_eq!(
            ok,
            serde_json::json!({
                "outcome": "success",
                "trigger": "user_command",
                "from_version": "0.2.118",
                "to_version": "0.2.120",
                "channel": "alpha",
                "installer": "internal",
                "platform": "macos-x86_64",
                "rosetta": true,
                "duration_ms": 12000,
            })
        );
        let fail = serde_json::to_value(CliUpdate {
            outcome: CliUpdateOutcome::Failed,
            trigger: CliUpdateTrigger::AutoBackground,
            from_version: "0.2.118".into(),
            to_version: Some("0.2.120".into()),
            channel: CliUpdateChannel::Alpha,
            installer: CliUpdateInstaller::Internal,
            platform: "macos-x86_64".into(),
            rosetta: true,
            duration_ms: 60_100,
            error_kind: Some(CliUpdateErrorKind::SmokeTimeout),
        })
        .unwrap();
        assert_eq!(fail.get("outcome").and_then(|v| v.as_str()), Some("failed"));
        assert_eq!(
            fail.get("error_kind").and_then(|v| v.as_str()),
            Some("smoke_timeout")
        );
        assert_eq!(
            fail.get("trigger").and_then(|v| v.as_str()),
            Some("auto_background")
        );
        assert!(fail.get("error").is_none());
        assert_eq!(
            serde_json::to_value(CliUpdateTrigger::LeaderConverge).unwrap(),
            "leader_converge"
        );
        // Trigger as_str / FromStr / serde are one rendering.
        for t in [
            CliUpdateTrigger::UserCommand,
            CliUpdateTrigger::AutoBackground,
            CliUpdateTrigger::LeaderConverge,
        ] {
            assert_eq!(serde_json::to_value(t).unwrap(), t.as_ref());
            assert_eq!(t.as_ref().parse::<CliUpdateTrigger>().unwrap(), t);
        }
        assert!("bogus".parse::<CliUpdateTrigger>().is_err());
        // Wire values and from_installer_str round-trip: one mapping
        for (installer, wire) in [
            (CliUpdateInstaller::Npm, "npm"),
            (CliUpdateInstaller::GhRelease, "gh-release"),
            (CliUpdateInstaller::Internal, "internal"),
            (CliUpdateInstaller::Other, "other"),
        ] {
            assert_eq!(serde_json::to_value(installer).unwrap(), wire);
            assert_eq!(CliUpdateInstaller::from_installer_str(wire), installer);
        }
        assert_eq!(
            CliUpdateInstaller::from_installer_str("homebrew"),
            CliUpdateInstaller::Other
        );
    }

    /// Private mirror names bucket to Other; empty means stable.
    #[test]
    fn cli_update_channel_buckets() {
        assert_eq!(
            CliUpdateChannel::from_channel_str(" alpha "),
            CliUpdateChannel::Alpha
        );
        assert_eq!(
            CliUpdateChannel::from_channel_str(""),
            CliUpdateChannel::Stable
        );
        assert_eq!(
            CliUpdateChannel::from_channel_str("stable"),
            CliUpdateChannel::Stable
        );
        assert_eq!(
            CliUpdateChannel::from_channel_str("enterprise"),
            CliUpdateChannel::Enterprise
        );
        for private in ["acme-mirror.1", "x'; rm -rf ~;'", "a b"] {
            assert_eq!(
                CliUpdateChannel::from_channel_str(private),
                CliUpdateChannel::Other,
                "{private:?} must bucket to other"
            );
        }
    }

    #[test]
    fn plugin_cta_installed_includes_error_category_when_some() {
        let v = serde_json::to_value(PluginCtaInstalled {
            plugin_name: "figma".into(),
            success: false,
            error_category: Some("not_found".into()),
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "plugin_name": "figma",
                "success": false,
                "error_category": "not_found",
            })
        );
    }
}
