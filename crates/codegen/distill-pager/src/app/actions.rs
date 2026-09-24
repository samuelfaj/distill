// Modified for Distill by Samuel Fajreldines, 2026.
//! Application actions, effects, and task results.
//!
//! This module defines the three enums that form the backbone of the pipeline from input event to dispatch to effect:
//!
//! - [`Action`]: produced by input handling, consumed by dispatch (sync).
//! - [`Effect`]: produced by dispatch, consumed by the event loop (async).
//! - [`TaskResult`]: produced by spawned tasks, fed back into dispatch.
use super::agent::AgentId;
use crate::app::status_line::StatusLineRun;
use crate::scrollback::entry::EntryId;
use agent_client_protocol as acp;
use distill_shell::sampling::types::ReasoningEffort;
use distill_shell::session::unified_list::SessionKind;
/// Independent provider logins leave Grok's ACP authentication intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginProvider {
    ChatGpt,
    OpenRouter,
}

impl LoginProvider {
    pub fn name(self) -> &'static str {
        match self {
            Self::ChatGpt => "ChatGPT",
            Self::OpenRouter => "OpenRouter",
        }
    }
}

/// Browser sign-ins are independent of the harness and of one another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderAuthState {
    pub grok: bool,
    pub chatgpt: bool,
    pub openrouter: bool,
}

/// Typed error for model switch failures.
/// Replaces the raw `String` in `TaskResult::SwitchModelComplete` so dispatch can match on the variant instead of parsing strings.
#[derive(Debug, Clone)]
pub enum SwitchModelError {
    /// The target model requires a different agent harness than the active session.
    /// The pager should offer to start a new session.
    /// Deserialized from `ModelSwitchIncompatibleAgentError` in `acp::Error.data`.
    IncompatibleAgent {
        error: distill_shell::agent::config::ModelSwitchIncompatibleAgentError,
        /// The model that was active before the optimistic UI update (if any).
        /// Used to roll back `models.current` when the user declines to start a new session.
        prev_model_id: Option<acp::ModelId>,
    },
    /// Any other failure (network, auth, server error, etc.).
    Other(String),
}
/// Synchronous, side-effect-free user intent.
/// Produced by [`super::input`] from key/mouse events.
/// Consumed by [`super::dispatch::dispatch`] to mutate state and return effects.
#[derive(Debug)]
#[cfg_attr(test, derive(strum::AsRefStr))]
#[allow(clippy::large_enum_variant)]
pub enum Action {
    /// Quit the application.
    Quit,
    /// Restart the binary to pick up a downloaded update.
    QuitForUpdate,
    /// Resume the recent foreign session offered on the launch welcome screen.
    ResumeForeignSession,
    /// Re-exec into the other screen mode (`true` means minimal).
    RelaunchInScreenMode {
        minimal: bool,
    },
    /// Quit without double-press confirmation (e.g., from command palette or pre-login screens).
    QuitConfirmed,
    /// Create a new session from the welcome screen.
    NewSession,
    /// Leave the welcome screen for the optimistic home session, or create one if none exists.
    LeaveHome,
    /// Ask whether the new session should use a git worktree.
    ChooseNewSessionMode,
    /// Exit the current session and return to the welcome screen.
    ExitSession,
    /// Exit session without double-press confirmation (e.g., from command palette).
    ExitSessionConfirmed,
    /// `/delete`: confirm, then delete history; return to welcome, or dashboard when attached.
    DeleteCurrentSession,
    DeleteCurrentSessionAnswered {
        confirmed: bool,
    },
    /// Open grok.com in the browser for SuperGrok subscription upsell.
    OpenSupergrokUrl,
    /// Re-check subscription status via the shell's `x.ai/auth/check_subscription`.
    CheckSubscription,
    /// Open an arbitrary URL in the system browser (with scheme validation).
    OpenUrl(String),
    /// Resubmit the prompt that last hit the credit/usage limit.
    RetryCreditLimitPrompt,
    /// Open a semantic scrollback link.
    OpenLink(crate::render::osc8::LinkTarget),
    /// Open grok.com managed connectors, appending session teamId when set.
    OpenManagedConnectors,
    /// Cycle to the next visible link (or highlight the first if none selected).
    OpenNextLink,
    /// Cycle to the previous visible link.
    OpenPrevLink,
    /// Fetch the session list for the session picker on the welcome screen.
    FetchSessionList,
    /// Cycle the active session picker's source filter.
    CycleSessionSourceFilter,
    /// Load a selected session from the session picker.
    PickSession(usize),
    /// Load a selected session from the session picker into a new worktree.
    PickSessionInWorktree(usize),
    /// Copy the selected session's ID to the clipboard.
    CopySessionId(usize),
    /// Toggle expanded card view for a session in the picker.
    ExpandSessionCard {
        source: String,
        session_id: String,
    },
    /// Open the session picker overlay (from within an active session via /resume).
    ShowSessionPicker,
    /// The session picker overlay was dismissed without a pick.
    /// Invalidate any in-flight list/search/foreign scan so a late response can't fall through to the welcome picker fields.
    SessionPickerClosed,
    /// Create a new session in a git worktree from the welcome screen.
    /// `load_session_id`: when `Some`, loads that session in the new worktree instead of creating a fresh one (`--resume` with `--worktree`).
    /// `label`: optional human-readable label from CLI `-w <label>` or dialog.
    NewWorktreeSession {
        load_session_id: Option<String>,
        label: Option<String>,
        /// Optional branch/tag/commit to base the worktree on (CLI `--ref` / `--worktree-ref`).
        git_ref: Option<String>,
    },
    /// Open the "New Worktree" popup dialog on the welcome screen.
    OpenNewWorktreeDialog,
    /// Open the interactive import-claude modal on the welcome screen.
    ImportClaudeSettings,
    /// User confirmed the import modal: apply selected items.
    ImportClaudeConfirm,
    /// User cancelled the import modal: close without applying.
    ImportClaudeCancel,
    /// Hide the import-claude menu row by recording the current `.claude/` content hash as "seen".
    /// Doesn't import anything, doesn't change runtime fallback behavior.
    /// The menu reappears only if `.claude/` content changes.
    DismissClaudeImport,
    /// Load (resume) an existing session by ID (strict: never create).
    /// `chat_kind` is the **conversation-entry** bit only (`source == "conversation"` / restore preserve), **not** sticky `--chat`.
    /// Under `--chat`, local Build disk rows are refused in dispatch (never coerced).
    LoadSession(String, Option<std::path::PathBuf>, bool),
    /// Welcome Local workspace ACK confirmed (y); write the ack and start the session.
    #[cfg(feature = "local-workspace")]
    ConfirmWelcomeLocalWorkspaceAck,
    /// Create a new session with a client-chosen session ID (`--session-id`).
    NewSessionWithId(String),
    /// Startup `--fork-session`: fork `parent` then load the child.
    /// Optional second string is the desired new session ID.
    StartupForkSession {
        parent_session_id: String,
        parent_cwd: Option<std::path::PathBuf>,
        new_session_id: Option<String>,
    },
    /// Send the current prompt text to the agent.
    SendPrompt(String),
    /// Post-turn plan revise: send notes without consuming the pre-review draft.
    /// [`Self::SendPrompt`] would wipe that draft and drain its images into the
    /// revision turn.
    RevisePlan(String),
    /// Submit a clicked follow-up suggestion chip as a LITERAL model prompt.
    /// The suggestion text is server/model-controlled, so it must bypass slash-command and exit-alias resolution.
    /// A `/always-approve` or `/quit` chip must never execute as a command.
    SubmitFollowUp(String),
    /// Execute a slash command without consuming the prompt textarea.
    /// Used by modal-driven slash dispatchers (command palette, ArgPicker) where the user's draft text in the prompt should survive the command.
    /// Behaves identically to `SendPrompt` (same registry resolution, same effect outputs) but skips the `prompt.set_text("")` calls.
    SendSlashCommandPreservingDraft(String),
    /// Send a mid-turn interjection without canceling the running turn.
    /// Reserved for text that answers the running turn (plan-review comments, permission follow-ups); user send-now takes [`Self::SendPromptNow`].
    Interject {
        text: String,
        /// Pasted images riding along with the interjection.
        /// Empty for producers that carry plain text (plan-review comments, etc.).
        images: Vec<crate::prompt_images::PastedImage>,
    },
    /// Approve a finished CreatePlan turn. Starts a new Agent turn via `ExecutePlanAction`.
    ExecutePlan {
        plan_file_content: String,
        /// File URI of the keep. Omitted on the wire when empty.
        plan_file_uri: Option<String>,
    },
    /// Cancel-and-send: cancel the running turn (background tasks and queued rows survive shell-side) and run this text as the next prompt turn.
    /// The send-now chord, empty-composer Enter on a queued local row, and the deferred-paste re-issue produce this.
    SendPromptNow {
        text: String,
        /// Pasted images riding along with the prompt.
        images: Vec<crate::prompt_images::PastedImage>,
    },
    /// Enable session voice mode and start recording (the Ctrl+Space hold-to-talk key-press, on terminals that report key releases).
    /// Start-only, never stops; use [`Self::VoiceStop`], [`Self::VoiceToggle`], or Esc to stop.
    EnableVoiceMode,
    /// Toggle capture (`/voice`, Ctrl+Space, Esc while listening, recording-row `[stop]`, and Ctrl+Space on terminals without key releases).
    /// Stops if recording, otherwise starts.
    VoiceToggle,
    /// Stop capture unconditionally (Ctrl+Space hold-to-talk key release).
    /// Clears any pending cold-start so a release during pipeline spawn can't leave a hot mic.
    VoiceStop,
    /// Send a direct bash command (bypasses agent loop).
    SendBashCommand(String),
    /// The user wiped a substantial prompt draft: show the seen-gated "ctrl+z to undo" ephemeral tip on the active agent.
    /// Gated by the per-tip `contextual_hints.undo` gate.
    ShowUndoTip,
    /// The user typed a planning keyword into the prompt: show the seen-gated "Planning? Check out plan mode via shift+tab" ephemeral tip.
    /// Gated by the per-tip `contextual_hints.plan_mode` gate.
    ShowPlanNudge,
    /// The user double-clicked scrollback while Text selection is fold/nav:
    /// show the seen-gated "/settings → Text selection → Word select" tip.
    /// Gated by the per-tip `contextual_hints.word_select` gate.
    ShowWordSelectTip,
    /// Accept the word-select tip (its advertised chord, pressed while the tip is on screen).
    /// Flip `keep_text_selection` to `word_select`, persist it, and retire the tip.
    AcceptWordSelectTip,
    /// Try to drain the next queued prompt (after editing completes, etc.).
    DrainQueue,
    /// Remove a server-authoritative (shared) queued prompt by its stable `prompt_id`.
    /// Routed to the agent as `x.ai/queue/remove`; the resulting `x.ai/queue/changed` rebroadcast is the source of truth.
    QueueRemoveShared {
        id: String,
        expected_version: u64,
    },
    /// Reorder the server-authoritative (shared) queued prompts to match `ordered_ids`. Routed as `x.ai/queue/reorder`.
    QueueReorderShared {
        ordered_ids: Vec<String>,
    },
    /// Clear the caller's server-authoritative (shared) queued prompts.
    /// Routed as `x.ai/queue/clear`.
    QueueClearShared,
    /// Replace the text of a server-authoritative (shared) queued prompt.
    /// Routed to the agent as `x.ai/queue/edit`; the rebroadcast of `x.ai/queue/changed` is the source of truth.
    /// Last write wins via the session actor's serialized mailbox; no client-side conflict resolution.
    QueueEditShared {
        id: String,
        new_text: String,
    },
    /// Hold a server-authoritative row out of combine-on-promote while editing.
    QueueHoldEditShared {
        id: String,
    },
    /// Release a previous [`Self::QueueHoldEditShared`].
    QueueReleaseEditShared {
        id: String,
    },
    /// Interject a server-authoritative (shared) queued prompt into the running turn.
    /// The agent atomically removes it from the queue and merges its text into the in-flight turn.
    /// Mirrors the local "Send now" / `Ctrl+Enter` path, which uses [`Interject`](Self::Interject) directly because the local queue is client-owned.
    QueueInterjectShared {
        id: String,
        expected_version: u64,
        /// Locally-edited replacement text (the edit-interject key while in `PromptMode::EditingQueued`).
        /// Without it the agent would interject the original server-side text, not the edit.
        /// Atomicity is documented on [`Effect::QueueInterject`].
        new_text: Option<String>,
    },
    /// A queued-row edit whose saved text is a complete pager builtin invocation: drop the row and run the command through the normal slash dispatch.
    /// The view only classifies; dispatch stays the sole execution owner.
    /// Dispatch removes the row only after its own guards pass, so a failed run leaves the row queued.
    RunEditedQueuedCommand {
        /// `PromptMode::EditingQueued.id`: the local `pending_prompts` id, or the synthesized selection id for a server row (unused there).
        local_id: u64,
        /// `Some` for a server-authoritative row.
        /// `None` covers both a local row and a server row that vanished from the mirror before Enter.
        /// With nothing to remove, no versioned `x.ai/queue/remove` request is sent.
        server: Option<SharedQueueTarget>,
        submission: crate::views::prompt_widget::StashedPrompt,
    },
    /// Resolution of the hard-modal blocked-prompt card.
    PromptBlockAnswered {
        /// The local queue row the blocked prompt was requeued into.
        row_id: u64,
        choice: PromptBlockChoice,
    },
    /// Focus the prompt pane.
    FocusPrompt,
    /// Focus the scrollback pane (leave prompt).
    FocusScrollback,
    /// Clear the prompt (history-aware).
    /// Armed by idle Esc double-press via [`super::app_view::InputOutcome::ArmPending`] (no ActionDef; not a keybinding).
    ClearPrompt,
    /// Focus the scrollback pane and open an incremental search over it.
    /// Drives the `/find` slash command so simple-mode users (where a bare `/` goes to the prompt) reach the same search as the vim `/` key.
    /// Carries the optional `/find <word>` argument to pre-fill the bar.
    OpenScrollbackSearch(Option<String>),
    /// Select next entry in scrollback.
    SelectNext,
    /// Select previous entry.
    SelectPrev,
    /// Jump to next turn boundary.
    NextTurn,
    /// Jump to previous turn boundary.
    PrevTurn,
    /// Jump to the next turn at the viewport top.
    NextResponse,
    /// Jump to the previous turn at the viewport top.
    PrevResponse,
    /// Scroll up by N lines.
    ScrollUp(u16),
    /// Scroll down by N lines.
    ScrollDown(u16),
    /// Go to top of scrollback.
    GotoTop,
    /// Go to bottom of scrollback.
    GotoBottom,
    /// Half page up.
    HalfPageUp,
    /// Half page down.
    HalfPageDown,
    /// Full page up.
    PageUp,
    /// Full page down.
    PageDown,
    /// Collapse selected entry (no-op if already collapsed or not foldable).
    Collapse,
    /// Expand selected entry (no-op if already expanded or not foldable).
    Expand,
    /// Toggle fold on selected entry.
    ToggleFold,
    /// Smart expand/collapse all: expand all if any collapsed, else collapse all.
    ToggleExpandAll,
    /// Expand all thinking blocks (toggle: expand if any collapsed, else collapse all).
    ExpandAllThinking,
    /// Toggle raw markdown on selected entry.
    ToggleRaw,
    /// Toggle terminal mouse reporting (mouse capture).
    /// Disabling it lets the terminal handle native click-drag text selection and copy-paste; re-enabling restores in-app mouse handling.
    /// Bound to Ctrl+R while the scrollback pane is focused.
    ToggleMouseCapture,
    /// Toggle the scroll-diagnostics HUD (hidden `/scroll-debug` command, also `/debug scroll`; `GROK_SCROLL_DEBUG=1` enables it from startup).
    ToggleScrollDebugHud,
    /// Toggle the release-safe FPS HUD (`/debug fps`).
    ToggleFpsHud,
    /// Toggle the scroll flight recorder at runtime (`/debug log`; `GROK_SCROLL_LOG=1` enables it from startup).
    ToggleScrollLog,
    /// Print the `/debug` toggles and their on/off state to the transcript.
    ShowDebugStatus,
    /// Copy selected block's content to clipboard.
    CopyBlockContent,
    /// Copy the Nth most recent assistant message (1 is the latest).
    /// `None` copies to the clipboard (with file fallback on failure); `Some(p)` writes a UTF-8 file.
    CopyAssistantMessage {
        n: usize,
        file_path: Option<std::path::PathBuf>,
    },
    /// Export the active (sub)agent's conversation transcript as Markdown.
    /// `None` copies to the clipboard (with route-aware toast and stats); `Some(p)` writes a UTF-8 file.
    /// All ~ expansion, parent dir creation, and fs::write live in the dispatch handler.
    ExportConversation {
        file_path: Option<std::path::PathBuf>,
    },
    /// Render the active (sub)agent's full transcript to a temp Markdown file and open it in `$PAGER` (default `less`).
    /// The inline TUI is suspended for the duration.
    /// The dispatch handler renders and writes the file and arms `AppView::pending_pager_path`; the event loop does the suspend/restore.
    OpenTranscriptPager,
    /// Minimal mode (`grok --minimal`): re-print the last committed folded block, fully expanded, into native scrollback below the conversation.
    /// Folded means collapsed reasoning or truncated tool output.
    /// Bound to `Ctrl+E` and the `/expand` command. No-op outside minimal mode or when nothing folded remains to expand.
    MinimalExpandLast,
    /// Copy selected block's metadata (e.g., command for execute blocks).
    CopyBlockMeta,
    /// Open the selected block in the fullscreen viewer.
    OpenBlockViewer,
    /// Open the extensions modal dialog on a specific tab.
    OpenExtensionsModal {
        tab: crate::views::extensions_modal::ExtensionsTab,
        trigger: distill_telemetry::events::ExtensionsModalTrigger,
    },
    /// Open the agents modal (listing all agent definitions).
    /// Optionally opens directly on a specific tab.
    OpenConfigAgentsModal(Option<crate::views::agents_modal::AgentsTab>),
    /// Trigger OAuth for an MCP server from the modal.
    McpAuthTrigger {
        server_name: String,
    },
    McpSetupSubmit {
        server_name: String,
        values: std::collections::HashMap<String, String>,
    },
    /// Reload skills list from the modal.
    ReloadSkills,
    /// Refresh MCP server list from the modal.
    RefreshMcpList,
    /// Execute a hooks management action from the modal.
    ExecuteHooksAction(distill_hooks_plugins_types::HooksAction),
    /// Execute a plugins management action from the modal.
    ExecutePluginsAction(distill_hooks_plugins_types::PluginsAction),
    /// Execute a marketplace management action from the modal.
    ExecuteMarketplaceAction(distill_hooks_plugins_types::MarketplaceAction),
    /// Add or update an MCP server via x.ai/mcp/upsert.
    UpsertMcpServer {
        name: String,
        config: Box<distill_shell::util::config::McpServerConfig>,
    },
    /// Delete an MCP server via x.ai/mcp/delete.
    DeleteMcpServer {
        server_name: String,
    },
    /// Live-toggle an MCP server (enable/disable without session restart).
    ToggleMcpServer {
        server_name: String,
        enabled: bool,
    },
    /// Toggle a skill enable/disable via x.ai/skills/toggle.
    ToggleSkill {
        skill_name: String,
        enabled: bool,
    },
    /// Toggle a single MCP tool within a server (enable/disable).
    ToggleMcpTool {
        server_name: String,
        tool_name: String,
        enabled: bool,
    },
    /// Cycle to next model.
    NextModel,
    /// Switch active model.
    SwitchModel {
        model_id: acp::ModelId,
        effort: Option<ReasoningEffort>,
    },
    /// `/effort auto`: let the decision layer choose the effort for every model
    /// call. The last explicit level stays the fallback when it abstains.
    SetEffortAuto {
        model_id: acp::ModelId,
    },
    /// Cancel the currently running turn.
    CancelTurn,
    /// User confirmed a cancel-turn choice from the panel.
    CancelTurnChoice(crate::views::modal::CancelTurnChoice),
    /// Kill a background task by task_id.
    KillBgTask(String),
    /// Kill (cancel) a subagent by subagent_id.
    KillSubagent(String),
    CancelScheduledTask(String),
    /// Demote the currently running execute tool to a background task.
    DemoteToBackground,
    /// Request current bundle cache status via `x.ai/bundle/status`.
    RequestBundleStatus,
    /// View a catalog entry's raw content in the block viewer.
    ViewCatalogEntry {
        kind: String,
        name: String,
    },
    /// Hide the announcements banner.
    AnnouncementsHide,
    /// Show the announcements banner.
    AnnouncementsShow,
    /// Open the promo CTA link (url resolved from current state at dispatch time, mirroring how `AnnouncementsHide` resolves its target).
    /// The payload records which UI element activated it, for telemetry.
    AnnouncementsOpenCta(distill_telemetry::events::AnnouncementCtaSurface),
    /// Cycle session mode (Shift+Tab): Normal, Plan, Auto, Always-Approve, then back to Normal (Auto skipped when the feature gate is off).
    /// Plan entered on top of a permission (Plan + Auto, Plan + Always-Approve) exits plan and keeps that permission.
    /// Plan mode sends a signal to the shell; always-approve is local.
    CycleMode,
    /// Toggle YOLO mode (auto-approve all permissions). Ctrl+O.
    ToggleYolo,
    /// Set YOLO (auto-approve / `always-approve`) mode.
    SetYoloMode(bool),
    /// Set the permission mode by canonical kind (`always-approve` / `ask` / `default`).
    /// Typed wrapper over [`Action::SetYoloMode`] that preserves the `default` canonical (the `bool` variant collapses `default` to `ask`).
    SetPermissionMode(PermissionModeKind),
    /// Toggle multiline input mode (swap Enter and Shift+Enter behavior).
    ToggleMultiline,
    /// Set multiline input mode (swap Enter and Shift+Enter behavior).
    /// Pager-owned, NOT persisted to disk; reset each session.
    SetMultilineMode(bool),
    /// Open the prompt-history search panel on the active agent (composer as filter query). Dispatched by `/history`.
    OpenHistorySearch,
    /// Set how ` ```mermaid ` code blocks are rendered (auto/on/off).
    /// SHELL-owned: updates the process-wide cache mirror and persists to `[ui].render_mermaid` in config.toml via `Effect::PersistSetting`.
    SetRenderMermaid(crate::appearance::RenderMermaid),
    /// Toggle vim-style scrollback keybindings (j/k, h/l, g/G, y/Y, etc.).
    /// Delegates to `set_vim_mode` so the new value is persisted to `[ui].vim_mode` in config.toml, the same path as the settings modal.
    ToggleVimMode,
    /// Set vim-style scrollback keybindings. SHELL-owned: persisted to `[ui].vim_mode` in config.toml via `Effect::PersistSetting`.
    /// Used by the settings modal; the `ToggleVimMode` variant covers the `/vim-mode` slash-command path.
    SetVimMode(bool),
    /// Toggle the per-tool "Always allow …" prompt options. SHELL-owned; persisted to `[ui].remember_tool_approvals`. Applies to new sessions.
    SetRememberToolApprovals(bool),
    /// Toggle the ask_user_question timeout. SHELL-owned; persisted to `[toolset.ask_user_question].timeout_enabled`. Applies to new sessions.
    SetAskUserQuestionTimeoutEnabled(bool),
    /// SHELL-owned `keep_text_selection` (`flash` | `hold`); cache and persist.
    SetKeepTextSelection(crate::appearance::TextSelection),
    /// Set the mouse-wheel scroll speed multiplier (1-100).
    /// Pager-owned ephemeral: process-wide cache, no `Effect::PersistSetting`.
    SetScrollSpeed(i64),
    /// Force scroll input classification (`auto` | `wheel` | `trackpad`).
    /// SHELL-owned: cache mirror and `[ui].scroll_mode` via `Effect::PersistSetting`.
    SetScrollMode(crate::appearance::ScrollMode),
    /// Invert vertical scroll direction. SHELL-owned: cache mirror and `[ui].invert_scroll` via `Effect::PersistSetting`.
    SetInvertScroll(bool),
    /// Set lines-per-tick for both wheel and trackpad (1-10). SHELL-owned: cache mirror and `[ui].scroll_lines` via `Effect::PersistSetting`.
    SetScrollLines(i64),
    /// Set whether agent thinking blocks are shown.
    /// SHELL-owned: updates the process-wide cache mirror and persists to `[ui].show_thinking_blocks` via `Effect::PersistSetting`.
    SetShowThinkingBlocks(bool),
    /// Set whether runs of consecutive non-destructive tool calls and subagent rows are grouped into one row.
    /// SHELL-owned: updates the process-wide cache mirror and persists to `[ui].group_tool_verbs` via `Effect::PersistSetting`.
    SetGroupToolVerbs(bool),
    /// Set whether Edit blocks default to the collapsed one-line diffstat summary.
    /// SHELL-owned: updates the process-wide cache mirror and persists to `[ui].collapsed_edit_blocks` via `Effect::PersistSetting`.
    SetCollapsedEditBlocks(bool),
    /// Set whether the predicted-next-prompt ghost text (tab autocomplete) is offered after each turn.
    /// SHELL-owned: updates the process-wide cache mirror and persists to `[ui].prompt_suggestions` via `Effect::PersistSetting`.
    SetPromptSuggestions(bool),
    /// Set `[scrollback.scroll].respect_manual_folds`.
    /// PAGER-owned: live-applied via `AppView::set_appearance` and persisted to pager.toml via `Effect::PersistSetting`.
    SetRespectManualFolds(bool),
    /// Set the canonical for `[ui].default_selected_permission`. Persists via `Effect::PersistSetting`.
    /// Payload is the registry's canonical string (`default` | `allow_once` | `allow_always` | `reject`).
    SetDefaultSelectedPermission(String),
    /// Set the hunk-tracker mode. Payload is the registry canonical string.
    SetHunkTrackerMode(String),
    /// Set default screen mode (`fullscreen` | `minimal`); restart-required.
    SetScreenMode(String),
    /// Enable/disable the Ctrl+Space / F8 voice-dictation shortcut. SHELL-owned; persisted to `[ui].voice_keybind_enabled`.
    /// Takes effect on the next keypress; `/voice` is unaffected.
    SetVoiceKeybindEnabled(bool),
    /// Set the voice capture mode (`toggle` | `hold`). SHELL-owned; persisted to `[ui].voice_capture_mode`.
    /// Takes effect for the next Ctrl+Space press.
    SetVoiceCaptureMode(String),
    /// Set the voice STT language (catalog code or `auto`). SHELL-owned; persisted to `[ui].voice_stt_language`.
    /// Takes effect for the next voice capture.
    SetVoiceSttLanguage(String),
    /// Toggle timestamp display on messages.
    ToggleTimestamps,
    /// Toggle compact mode (reduce user message padding).
    ToggleCompactMode,
    /// Set compact mode (reduce user message padding).
    SetCompactMode(bool),
    /// Set timestamp display on messages.
    SetTimestamps(bool),
    /// Set timeline sidebar visibility (per-turn tick rail).
    SetTimeline(bool),
    /// Set `[ui].page_flip_on_send` (default ON). Persists via `Effect::PersistSetting`.
    SetPageFlipOnSend(bool),
    /// Set `[ui].confirm_before_rewind` (default ON). Persists via `Effect::PersistSetting`.
    SetConfirmBeforeRewind(bool),
    /// Set whether the drain call site merges the run of leading queued `Prompt` entries into one turn instead of sending them one by one.
    /// SHARED-owned: updates the process-wide cache mirror (read by the drain site).
    /// Persists to `[ui].combine_queued_prompts` via `Effect::PersistSetting`.
    SetCombineQueuedPrompts(bool),
    /// Mid-turn follow-up routing (`queue` | `steer`).
    /// SHARED-owned: `[ui].follow_up_behavior`.
    SetFollowUpBehavior(crate::appearance::FollowUpBehavior),
    /// Set simple mode (ASCII / minimal glyphs). Persists via `Effect::PersistSetting`.
    SetSimpleMode(bool),
    /// Set the per-tip contextual-hint user config (`[ui.contextual_hints]`).
    /// Each persists via `Effect::PersistSetting`.
    /// Each also immediately re-resolves and re-propagates the gates to every agent's prompt (runtime live-apply).
    SetContextualHintUndo(bool),
    SetContextualHintPlanMode(bool),
    SetContextualHintImageInput(bool),
    SetContextualHintSendNow(bool),
    SetContextualHintSmallScreen(bool),
    SetContextualHintWordSelect(bool),
    SetContextualHintExportCopy(bool),
    SetContextualHintSshWrap(bool),
    /// Commit the active theme (canonical name, e.g. `"groknight"`, `"auto"`).
    SetTheme(String),
    /// Commit the theme used when the OS is in dark mode.
    /// Only updates the live display when `theme = "auto"` AND system is in dark mode.
    SetAutoDarkTheme(String),
    /// Commit the theme used when the OS is in light mode.
    SetAutoLightTheme(String),
    /// Select the main model: switch the active session and persist `[models].default`.
    SetDefaultModel(acp::ModelId),
    /// Clear the persisted default model (`cfg.models.default = None`).
    /// Active session's model is unchanged; next session resolves via the shell's default-resolution chain.
    ClearDefaultModel,
    /// Save the optional reasoning model (`[models].reasoning`) without switching the main model.
    SetReasoningModel(acp::ModelId),
    /// Remove the reasoning model: the main model works alone.
    ClearReasoningModel,
    /// Commit the max-thoughts-width (column budget for the thoughts panel).
    /// Payload is `i64`; clamped to `u16` at the shell helper boundary.
    SetMaxThoughtsWidth(i64),
    /// Commit the fork-secondary model. Typed `ModelId` payload, persisted to `[ui].fork_secondary_model`.
    /// Rebroadcast via `ConfigUpdate::Ui` so running agents pick up the change.
    SetForkSecondaryModel(acp::ModelId),
    /// Clear the persisted fork-secondary model, restoring the built-in default.
    /// Active agent keeps its value; next fork uses the default.
    ClearForkSecondaryModel,
    /// Commit which `[model.<id>]` entry serves the utility model, persisted to
    /// `[jev.local].model`. Empty string clears the pick.
    /// Published after the config write succeeds.
    SetCheapModel(String, Option<ReasoningEffort>),
    /// Commit the `show_tips` preference. Persisted to `[cli].show_tips`.
    /// Restart-required: tips are resolved once at startup.
    SetShowTips(bool),
    /// Commit the `auto_update` preference. Persisted to `[cli].auto_update`.
    /// Restart-required: auto-update check fires once at startup.
    SetAutoUpdate(bool),
    /// Commit `[ui.display_refresh].auto_cadence_enabled`. Restart-required: cadence is pinned once at startup.
    SetDisplayRefreshAutoCadence(bool),
    /// Preview a theme without persisting; updates the live display only.
    /// Used by the picker on Up/Down and Esc (revert).
    PreviewTheme(String),
    /// Preview `auto_dark_theme` without persisting.
    /// Only applies when `theme = "auto"` AND system is in dark mode.
    PreviewAutoDarkTheme(String),
    /// Preview `auto_light_theme` without persisting.
    PreviewAutoLightTheme(String),
    /// Open the settings modal (F2, `/settings`, command palette).
    /// If already open, closes it instead of stacking.
    OpenSettings,
    /// Open settings on a registry key: its chooser, or the browse row when the setting is locked.
    OpenSettingsFocus {
        key: &'static str,
    },
    /// Privacy banner `[Opt in]` (ack only after ACP success).
    PrivacyBannerOptIn,
    /// Privacy banner `[Opt out]` (always writes the decline; ack only after ACP success).
    PrivacyBannerOptOut,
    /// Open the command palette (`/help`).
    /// The keybinding path (Ctrl+P) opens it directly in `handle_agent_action`; this lets a slash command reach the same modal through dispatch.
    OpenCommandPalette,
    /// Open the in-TUI How-to Guides doc picker (`/docs`, palette "How-to Guides").
    OpenHowtoGuides,
    /// Open the onboarding tutorial overlay (`/tutorial` or the command palette).
    OpenTutorial,
    /// Open the four-step first-run onboarding flow (`/onboarding`).
    OpenOnboarding,
    /// Cancel the Grok login started from the onboarding flow while staying on its step.
    CancelOnboardingLogin,
    /// Stop waiting for an independent provider login started from onboarding.
    CancelOnboardingProviderLogin(LoginProvider),
    /// Open the onboarding completion write; the overlay closes only after the write succeeds.
    CompleteOnboarding,
    /// Open the reset-settings confirmation dialog for a specific key.
    /// Moves the Settings modal state into `ResetSettingsConfirm` so the underlying modal survives the confirm dialog.
    OpenResetConfirm {
        key: crate::settings::SettingKey,
    },
    /// Resolve the reset-settings confirmation modal.
    /// `Reset` dispatches the default value via the typed `SetX` action.
    /// `Cancel` restores the underlying Settings modal unchanged.
    ConfirmResetSetting {
        choice: crate::views::modal::ResetSettingsResult,
    },
    /// Dump the input flight recorder to a debug file.
    DumpInputLog,
    /// User selected a permission option (AllowOnce, AllowAlways, RejectAlways).
    PermissionSelect(acp::PermissionOptionId),
    /// User typed a followup message and submitted it (RejectOnce with message).
    PermissionFollowup(String),
    /// User cancelled the front permission request (Ctrl-C / Esc in Options mode).
    PermissionCancel,
    /// Log out: remove credentials and return to the login screen.
    Logout,
    /// Log out and immediately start a new login flow.
    SwitchAccount,
    /// User pressed login on the welcome screen.
    Login,
    LoginProvider(LoginProvider),
    LogoutProvider(LoginProvider),
    /// Cancel an in-progress login that was started from inside a session (`/login` or a 401 re-auth prompt) and return to the previous view.
    /// Distinct from `Quit`: abandoning a mid-session re-auth must not exit the app or lose the open session.
    CancelLogin,
    /// User submitted a manually-pasted auth token (loopback mode).
    SubmitAuthCode(String),
    /// Copy the auth URL to the clipboard during authentication.
    CopyAuthUrl,
    /// Show the raw auth URL with mouse capture disabled for manual copy.
    ShowRawAuthUrl,
    /// Hide the raw auth URL and re-enable mouse capture.
    HideRawAuthUrl,
    /// Persist the shown workspace key. `Done` only after a durable write, an already-durable key, or auto-trust. A process-local persist stays `Pending`.
    /// already-durable key, or auto-trust. A process-local persist stays `Pending`.
    /// (Declining quits via [`Action::Quit`]; there is no decline action.)
    TrustFolder,
    /// Replays any session startup deferred behind the notice.
    /// (Declining quits via [`Action::Quit`]; there is no decline action.)
    AcceptConsent,
    /// Opens the notice's nth link. The url is re-read from validated state, never carried here.
    OpenConsentLink(usize),
    /// A spawned task completed.
    TaskComplete(TaskResult),
    /// Share the current session via URL.
    ShareSession,
    /// Show session info (auth, ID, cwd, model, context usage) instantly.
    ShowSessionInfo,
    /// Show release notes in a modal.
    ShowReleaseNotes {
        title: String,
        content: String,
    },
    /// Open the editable main, reasoning, and utility model form.
    ShowTierEditor,
    /// Save all three model tier fields from the tier editor.
    SetTierEditor {
        main: String,
        reasoning: String,
        utility: String,
    },
    /// Rename the current session's title/summary.
    RenameSession {
        title: String,
    },
    /// Unpin the current session title (`/rename --auto`).
    ResetSessionTitleToAuto,
    /// Show detailed context usage (progress bar, token breakdown, stats).
    ShowContextInfo,
    /// `/usage`: session token/cost, plus consumer credits when visible.
    ShowUsage,
    /// `/usage manage`: open consumer billing (a no-op when billing is hidden).
    ManageBilling,
    /// Commit a read-only list of the queued prompts as a system block (`/queue`).
    /// This is what minimal mode uses in place of the `QueuePane`.
    ShowQueue,
    /// Commit a read-only list of background tasks, subagents, and scheduled tasks as a system block (`/tasks`).
    /// This is what minimal mode uses in place of the `TasksPane`.
    ShowTasks,
    /// Show the current plan: preview popover if exists, toast if not.
    ShowPlan,
    /// Enter plan mode. If a description is provided, also start a turn with that text as the prompt.
    EnterPlanMode {
        description: Option<String>,
    },
    /// Set plan mode on/off. Per-session, ACP-mediated (not persisted to config.toml).
    /// `/plan <desc>` uses `EnterPlanMode` instead because it also starts a turn.
    SetPlanMode(PlanModeKind),
    /// Open the feedback modal (every screen mode).
    /// The payload's images were drained at slash-execution time; the modal composer adopts them as chips.
    OpenFeedbackModal(crate::views::feedback_modal::OpenFeedbackModal),
    /// Submit the open feedback modal's report.
    /// `modal_id` guards a deferred submit (paste probe in flight) against a modal that closed and reopened in between.
    SubmitFeedbackModal {
        modal_id: crate::views::feedback_modal::FeedbackModalId,
    },
    /// Execute one modal-scoped draft list/get/delete request.
    RequestFeedbackDraft {
        request: crate::views::feedback_modal::FeedbackDraftRequest,
    },
    /// Submit feedback immediately (inline `/feedback <text>` in any mode).
    /// `trace` is `None` when no trace consent was collected.
    SendFeedback {
        text: String,
        images: crate::views::prompt_widget::FeedbackImages,
        trace: Option<FeedbackTraceChoice>,
    },
    /// Enter remember mode (visual prompt change, not a send).
    EnterRememberMode,
    /// Send a remember note from # mode.
    /// Routes through LLM rewrite when a session is active; falls back to direct save otherwise.
    SendRememberNote(String),
    /// Save the currently displayed remember note from the review modal.
    SaveRememberNoteFromModal,
    /// Send a /btw side question (bypasses queue, works while agent is busy).
    /// `images` are composer attachments drained at submit; empty keeps the text-only wire.
    SendBtw {
        question: String,
        images: Vec<crate::prompt_images::PastedImage>,
    },
    /// Request a session recap ("where was I" summary).
    /// `auto` is `true` for the automatic return-from-away recap, `false` for an explicit `/recap`.
    /// Bypasses the prompt queue (works while the agent is busy).
    SendRecap {
        auto: bool,
    },
    /// Pick a session from content (deep search) results.
    PickContentSession {
        session_id: String,
        cwd: String,
    },
    /// Pick a session from content (deep search) results and resume in a worktree.
    PickContentSessionInWorktree {
        session_id: String,
        cwd: String,
    },
    /// Delete a session from history (local and remote).
    /// Fired from the session picker: `d` arms delete confirmation on the focused row, then `y` confirms (or `n`/other cancels).
    DeleteSession {
        source: String,
        session_id: String,
        cwd: String,
    },
    /// Trigger a deep content search for sessions matching the picker query.
    TriggerDeepSearch,
    /// Force an immediate deep content search, skipping the debounce.
    ForceDeepSearch,
    SetCodingDataSharing {
        opted_in: bool,
    },
    /// `/fork` slash command: parsed args produced by [`crate::slash::commands::fork::parse_fork_args`].
    /// The dispatcher resolves the worktree question (via flag or the local QuestionView modal) before constructing the placeholder.
    Fork(crate::slash::commands::fork::ForkArgs),
    /// Submit-path action emitted by the local fork worktree question modal.
    /// Routes directly to `dispatch_fork_resolved`.
    ForkAnswered {
        worktree: bool,
        directive: Option<String>,
        /// When `Some`, also persist this worktree mode preference so future `/fork` invocations skip the popup.
        persist_mode: Option<crate::app::app_view::WorktreeMode>,
    },
    /// Submit-path action emitted by the local `/new` worktree question modal.
    /// `worktree: true` creates the new session in a worktree; `worktree: false` creates it in the current cwd.
    NewSessionAnswered {
        worktree: bool,
        /// When `Some`, also persist this worktree mode preference so future `/new` invocations skip the popup.
        persist_mode: Option<crate::app::app_view::WorktreeMode>,
    },
    /// Answer from the agent-type-mismatch question modal.
    /// Shown when the shell rejects a model switch because the target model requires a different agent harness.
    AgentTypeMismatchAnswered {
        /// `true` starts a new session with the target model; `false` cancels and returns to the current session.
        start_new: bool,
        model_id: acp::ModelId,
        effort: Option<ReasoningEffort>,
    },
    DoctorFixConfirmed {
        target: DoctorFixTarget,
        plan: Box<crate::diagnostics::FixPlan>,
    },
    DoctorFixCancelled(DoctorFixTarget),
    /// Persist the memory modal fullscreen preference to config.toml.
    PersistMemoryFullscreen(bool),
    /// Turn memory on or off for the active session (`t` in the `/memory` modal).
    MemoryToggle {
        enabled: bool,
    },
    /// Copy text from the `/memory` modal; the outcome is shown in the modal's status line.
    MemoryCopy {
        text: String,
    },
    /// Delete one note from the `/memory` modal; the shell verifies the hash before removing.
    MemoryForget {
        path: String,
        expected_content_hash: String,
    },
    /// Open the Agent Dashboard view (`/dashboard`, `Ctrl+\`, `grok dashboard`).
    OpenDashboard,
    /// Close the dashboard, returning to the previous `ActiveView`.
    ExitDashboard,
    /// Attach to a dashboard row: switches to the parent agent and (for subagent rows) sets the parent's `active_subagent`.
    DashboardAttach(crate::views::dashboard::DashboardRowId),
    DashboardCloseSessionPicker,
    DashboardPickSession(usize),
    /// Submit the dispatch-input contents: either a filter, a new top-level agent, or a no-op when too short.
    /// `attach` mirrors Ctrl+S (dispatch and attach, "send and open").
    DashboardDispatch {
        text: String,
        attach: bool,
    },
    /// Submit a slash command from the dashboard's dispatch input. The text starts with `/`.
    /// The dispatcher resolves it through the slash registry (builtin / ACP / unknown) without a per-agent session context.
    /// Commands like `/dashboard`, `/exit`, `/theme`, `/settings`, `/help` work without a session; commands that need one return a friendly toast.
    DashboardDispatchSlash {
        text: String,
    },
    /// Pin/unpin the currently selected row.
    DashboardTogglePin,
    /// Enter inline-rename mode on the selected row.
    DashboardBeginRename,
    /// Commit a rename draft. Empty draft cancels.
    DashboardCommitRename,
    /// Cancel an in-progress rename without committing.
    DashboardCancelRename,
    /// Ctrl+X on the selected row.
    /// Top-level: cancels a running turn on a busy row, else double-press permanently deletes an idle row.
    /// Subagent: kills the subagent.
    DashboardStop,
    /// Confirm permanent delete of the armed dashboard row.
    DashboardDelete,
    /// Cycle the dispatch input's mode for the next spawned agent: Normal, Plan, Auto, Always-Approve, then back around.
    /// Auto is skipped when gated off. Bound to Shift+Tab.
    DashboardCycleMode,
    /// Cycle the PEEKED agent's live mode (same gated rotation as the agent prompt): the peek-panel counterpart to [`Self::DashboardCycleMode`].
    /// Unlike that staged dispatch mode, this changes the existing agent directly (same effect as Shift+Tab inside the agent's chat view).
    /// Emitted when Shift+Tab fires while the peek panel is open.
    DashboardPeekCycleMode,
    /// Toggle grouping between State and Directory.
    DashboardToggleGrouping,
    /// Set the live filter (typically driven by the dispatch input).
    DashboardSetFilter(crate::views::dashboard::FilterValue),
    /// Move selection cursor one row down.
    DashboardSelectNext,
    /// Move selection cursor one row up.
    DashboardSelectPrev,
    /// Reorder the selected row one slot up (Shift+↑).
    DashboardReorderUp,
    /// Reorder the selected row one slot down (Shift+↓).
    DashboardReorderDown,
    /// Exit the dashboard's session-overlay (an attached agent view whose header row carries `‹ i/n ›` and `[Dashboard]`).
    /// Returns to the dashboard with the cursor on the previously attached row.
    /// Bound to Esc, Ctrl+\\, and `[Dashboard]` click inside the overlay.
    DashboardOverlayExit,
    /// Cycle the dashboard's session-overlay to the previous top-level agent in the row list (`‹` click or Ctrl+\[).
    DashboardOverlayPrev,
    /// Cycle the dashboard's session-overlay to the next top-level agent in the row list (`›` click or Ctrl+\]).
    DashboardOverlayNext,
    /// Confirmed stop from inside the dashboard's session-overlay: close the attached session and return to the dashboard.
    /// State machine documented at `dispatch_dashboard_overlay_stop`.
    DashboardOverlayStop,
    /// Toggle auto-approve (YOLO mode) on the selected row's owning agent.
    /// Mirrors `Action::ToggleYolo` but targets the dashboard's selected row instead of the active-view agent.
    DashboardToggleAutoApprove,
    /// Toggle worktree-dispatch mode: when on, the next agent dispatched from the dashboard spawns in a fresh git worktree.
    /// The `+ New Agent` button then reads `+ New Agent in Worktree`. Bound to Ctrl+W.
    /// Worktrees require a git repo, so outside one the toggle is a no-op with an explanatory toast.
    DashboardToggleWorktree,
    /// Open the dashboard's shortcuts cheatsheet modal.
    /// It is the same searchable picker as the agent view's `ShortcutsHelp`, scoped to dashboard-context bindings.
    /// Bound to Ctrl+. (default) and `?` (alt).
    DashboardOpenShortcutsHelp,
    /// Close the dashboard's shortcuts cheatsheet modal.
    /// Routed from the modal-chrome `CloseRequested` outcome and from Esc when the modal is open.
    DashboardCloseShortcutsHelp,
    /// Focus the actions row's `+ New Agent` button.
    /// Mirrors a row-selection action: the button becomes the cursor target and the previous row selection (if any) clears.
    /// The dispatch input's placeholder flips back to the new-session form.
    DashboardFocusNewAgentButton,
    /// Create a new session AND open its detail view.
    /// Routed from `+ New Agent` click and from Enter-on-empty-prompt while the button is focused.
    /// Distinct from `DashboardDispatch`, which queues a prompt and stays on the dashboard.
    DashboardCreateNewAgentWithDetail,
    /// Open the dashboard's location picker: a floating modal that lists recent project directories (plus the current cwd) and accepts a typed path.
    /// It lets the user change where newly dispatched sessions run.
    /// Bound to Ctrl+L (default), a click on the header location label, and `/cd` with no argument.
    DashboardOpenLocationPicker,
    /// Close the dashboard's location picker modal without changing the working directory.
    /// Routed from Esc, the modal-chrome `CloseRequested` outcome, and a click outside the modal.
    DashboardCloseLocationPicker,
    /// Change the working directory for newly dispatched dashboard sessions.
    /// `input` is the raw path text: a picker row's path, a path typed into the picker's query field, or the `/cd <path>` argument.
    /// The dispatcher resolves `~` / relative paths against `app.cwd` and validates the target is a directory.
    DashboardChangeLocation {
        input: String,
    },
    /// Confirm the dashboard worktree-label dialog: create the next dashboard agent in a fresh git worktree rooted at `app.cwd`.
    /// `label` names the worktree; `None` auto-generates one.
    /// Any prompt stashed when the dialog opened (a prompt-send) is replayed into the new agent.
    DashboardConfirmWorktree {
        label: Option<String>,
    },
    DashboardPermissionSelect {
        row: crate::views::dashboard::DashboardRowId,
        request_id: usize,
        option_id: acp::PermissionOptionId,
    },
    /// Reject the peeked agent's pending permission with a typed feedback message (the peek panel's "No, type to add feedback" path).
    /// Resolves the front request with the `RejectOnce` option and the text attached as `followup_message` meta.
    /// Mirrors the agent view's `PermissionFollowup`. `request_id` guards against a stale answer if the queue rotated.
    DashboardPermissionFollowup {
        row: crate::views::dashboard::DashboardRowId,
        request_id: usize,
        text: String,
    },
    /// Answer the peeked agent's pending `AskUserQuestion` (the Ask tool) from the dashboard peek panel.
    /// `option_idx` selects an option; `None` with non-empty `freeform` submits the "Other" free-text answer.
    /// Only valid for a single-question, single-select ext ask.
    DashboardQuestionAnswer {
        row: crate::views::dashboard::DashboardRowId,
        option_idx: Option<usize>,
        freeform: String,
    },
    /// Send or queue a reply to the peeked agent from the dashboard peek panel's `❯ reply` input.
    /// The reply targets `row`'s owning top-level agent.
    /// When it is idle the prompt is sent immediately (a turn starts); mid-turn it is queued and drains after the current turn finishes.
    DashboardPeekReply {
        row: crate::views::dashboard::DashboardRowId,
        text: String,
        attach: bool,
    },
    /// Open the memory browser modal.
    OpenMemoryModal,
    /// Open the hidden `/gboom` easter egg (DOOM-style raycaster modal).
    OpenGboom,
    /// Suspend the TUI and open a configuration file in `$EDITOR`.
    SuspendForEditor {
        path: std::path::PathBuf,
        /// Reload `/config-agents` list after the editor exits (when set).
        refresh_agents_modal: Option<crate::views::agents_modal::AgentsTab>,
    },
    /// Edit the current minimal-mode composer draft in an external editor.
    EditPromptExternal,
    /// Toggle the expanded goal detail overlay.
    ToggleGoalDetail,
    ToggleWorkflows,
    Rewind,
    RewindShowPicker,
    RewindPickerSelect(usize),
    RewindConfirm(usize),
    /// Confirm rewind and turn off `confirm_before_rewind` for future rewinds.
    RewindConfirmNeverAsk(usize),
    RewindCancelOffer,
    RewindDismiss,
    RewindDismissError,
    /// Submit an inline edit: conversation-only rewind to that prompt, then resubmit the edited text (state lives on `AgentView::inline_edit`).
    InlineEditSubmit,
    /// Open the `/jump` turn picker.
    JumpShowPicker,
    /// Jump to a turn by its prompt's stable id and close the picker.
    JumpPickerSelect(EntryId),
    /// Close the picker and restore the stashed viewport.
    JumpDismiss,
}
/// A server-authoritative queue row plus the version its removal is checked against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SharedQueueTarget {
    pub id: String,
    pub expected_version: u64,
}
/// Persist-and-notify behavior for [`Effect::PersistPermissionMode`].
/// Both variants write to `~/.grok/config.toml` and route ACP
/// `x.ai/yolo_mode_changed` notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionModePersist {
    /// Typed-setter path: on disk-write failure, revert in-memory state to the prior canonical (`&'static str`).
    /// ACP notification is suppressed on failure so the agent never sees the optimistic value.
    /// The soft-default latch is NOT restored; a failed persist leaves the mode user-claimed until restart, matching the cycle path.
    WithRollback(&'static str),
    /// Cycle-mode path: no clean single-field rollback.
    /// On disk failure, logs a warning and leaves in-memory state at the optimistic value.
    /// ACP notification fires unconditionally.
    BestEffort,
}
/// Canonical permission-mode state for the `permission_mode` setting.
/// `Default` and `Ask` both project onto `yolo_mode = false` at runtime but are distinct on disk.
/// `Auto` uses the LLM classifier (not full always-approve).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionModeKind {
    /// Agent's default behavior (prompt). `yolo_mode = false`.
    Default,
    /// Explicit prompt-every-time. `yolo_mode = false`.
    Ask,
    /// LLM classifier for non-fast-path tools. `yolo_mode = false`, `auto_mode = true`.
    Auto,
    /// Auto-approve all tool actions. `yolo_mode = true`.
    AlwaysApprove,
}
impl PermissionModeKind {
    /// Canonical persisted/wire string for the kind.
    /// Matches the `EnumChoice.canonical` values in `settings/defs.rs::PERMISSION_MODE_CHOICES`.
    pub fn as_canonical(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::AlwaysApprove => "always-approve",
        }
    }
    /// Bool projection onto the YOLO runtime flag: `AlwaysApprove` is `true`, everything else `false`.
    /// Used by `set_yolo_mode_inner` to perform the actual state mutation without caring about the canonical distinction.
    /// That mutation covers `agent.session.yolo_mode`, `app.default_yolo`, and the permission_queue drain.
    pub fn is_always_approve(self) -> bool {
        matches!(self, Self::AlwaysApprove)
    }
    /// LLM classifier mode (distinct from always-approve).
    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }
    /// Construct from a canonical string. Returns `None` for unknown strings.
    /// Used by `apply_setting_rollback("permission_mode", _)` to recover the typed kind from the `SettingValue::Enum(canonical)` rollback payload.
    pub fn from_canonical(s: &str) -> Option<Self> {
        match s {
            "default" => Some(Self::Default),
            "ask" => Some(Self::Ask),
            "auto" => Some(Self::Auto),
            "always-approve" => Some(Self::AlwaysApprove),
            _ => None,
        }
    }
}
/// No `Default` arm, unlike [`PermissionModeKind`]: the runtime flags cannot tell `default` from `ask`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionLabel {
    Ask,
    Auto,
    AlwaysApprove,
}
impl From<PermissionLabel> for PermissionModeKind {
    fn from(label: PermissionLabel) -> Self {
        match label {
            PermissionLabel::Ask => Self::Ask,
            PermissionLabel::Auto => Self::Auto,
            PermissionLabel::AlwaysApprove => Self::AlwaysApprove,
        }
    }
}
impl PermissionLabel {
    /// Shares [`PermissionModeKind::as_canonical`] so the info-line flags and the scrollback rows use one string table.
    pub fn as_canonical(self) -> &'static str {
        PermissionModeKind::from(self).as_canonical()
    }
}
impl std::fmt::Display for PermissionLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_canonical())
    }
}
#[cfg(test)]
mod permission_mode_kind_tests {
    use super::PermissionModeKind;
    #[test]
    fn auto_is_distinct_from_always_approve() {
        let auto = PermissionModeKind::Auto;
        assert_eq!(auto.as_canonical(), "auto");
        assert!(!auto.is_always_approve());
        assert!(auto.is_auto());
        assert_eq!(
            PermissionModeKind::from_canonical("auto"),
            Some(PermissionModeKind::Auto)
        );
        assert_ne!(
            PermissionModeKind::Auto.as_canonical(),
            PermissionModeKind::AlwaysApprove.as_canonical()
        );
    }
    #[test]
    fn permission_mode_choices_include_auto_in_catalog() {
        for c in ["default", "ask", "auto", "always-approve"] {
            assert!(
                PermissionModeKind::from_canonical(c).is_some(),
                "catalog canonical {c} must parse"
            );
        }
    }
}
/// What the user chose on the hard-modal blocked-prompt card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptBlockChoice {
    Edit,
    Resend,
    Discard,
}
/// Which surface issued a feedback POST, echoed back on its completion.
/// Every send thanks at send time; the [`FeedbackSubmissionId`](crate::views::feedback_modal::FeedbackSubmissionId)
/// only correlates a completion with the consent parked for that exact POST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackSendOrigin {
    /// Inline `/feedback <text>`: fire-and-forget.
    Immediate,
    /// A feedback modal submit; the id correlates the completion with that exact POST attempt.
    Modal {
        submission_id: crate::views::feedback_modal::FeedbackSubmissionId,
        modal_id: crate::views::feedback_modal::FeedbackModalId,
        is_draft: bool,
    },
}
#[derive(Debug, Clone)]
pub struct DraftFeedbackBody {
    pub draft_id: distill_feedback::FeedbackDraftId,
    pub title: String,
    pub details: String,
    pub area: Option<String>,
    pub r#type: distill_feedback::FeedbackType,
    pub task_category: Option<distill_feedback::FeedbackTaskCategory>,
    pub failure_mode: Option<distill_feedback::FeedbackFailureMode>,
    pub images: Vec<distill_shell::session::FeedbackImage>,
}
/// What the user chose on the legacy `/feedback` trace-consent card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackTraceChoice {
    AlwaysUpload,
    NoUpload,
    NeverAsk,
}
/// Canonical on/off state for `plan_mode`.
/// Binary today (single bit on `agent.plan_mode_active`); typed enum so a future third state can be added without churning dispatcher arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanModeKind {
    /// Agent in `SessionMode::Plan`: no tool writes, plan-first.
    On,
    /// Agent in `SessionMode::Default`.
    Off,
}
impl PlanModeKind {
    /// Canonical persisted/wire string for the kind.
    /// Matches the `EnumChoice.canonical` values in `settings/defs.rs::PLAN_MODE_CHOICES`.
    pub fn as_canonical(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }
    /// Display label used in the toast and the picker. Mirrors `EnumChoice.display`.
    pub fn as_display(self) -> &'static str {
        match self {
            Self::On => "On",
            Self::Off => "Off",
        }
    }
    /// Bool projection: `On` is `true`, `Off` is `false`.
    /// Used by the dispatcher's idempotency check against `plan_mode_active`.
    pub fn to_bool(self) -> bool {
        matches!(self, Self::On)
    }
    /// Construct from a bool (the inverse of [`Self::to_bool`]).
    pub fn from_bool(b: bool) -> Self {
        if b { Self::On } else { Self::Off }
    }
}
/// What user gesture triggered a turn cancel; sent as `session/cancel`'s `_meta.cancelTrigger`.
/// The shell's deny-list treats every gesture value as a stop, so new variants need no shell change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelTrigger {
    /// `Ctrl+C` pressed (the default cancel keybinding). A bare Esc never cancels; it only hints at this key.
    CtrlC,
    /// The on-screen cancel button was clicked.
    Mouse,
    /// The dashboard's stop key (Ctrl+X), from the overlay or a busy row, downgraded to a turn cancel.
    DashboardStop,
}
impl CancelTrigger {
    /// Snake_case wire string sent as `_meta.cancelTrigger`.
    pub fn as_wire_str(self) -> &'static str {
        match self {
            Self::CtrlC => "ctrl_c",
            Self::Mouse => "mouse",
            Self::DashboardStop => "dashboard_stop",
        }
    }
}
/// Where a deferred clipboard-attachment paste lands once the off-thread probe finishes (see [`Effect::ProbeClipboardAttachment`]).
#[derive(Debug, Clone)]
pub enum ClipboardPasteTarget {
    /// Agent prompt input.
    /// `images_dir` is the session images dir used for the off-thread persist (`None` when no session exists yet).
    AgentPrompt {
        agent_id: AgentId,
        images_dir: Option<std::path::PathBuf>,
    },
    /// Dashboard new-session dispatch input.
    DashboardDispatch,
    /// Dashboard peek reply input. `row` is the peeked row at enqueue time.
    /// The completion drops the paste if the panel closed or moved to another row, so the attachment can't land in a different agent's reply.
    DashboardPeek {
        row: crate::views::dashboard::DashboardRowId,
    },
    /// Feedback modal composer input. Both ids are captured at enqueue time, so a completion is
    /// dropped after either the modal or its loaded composition changes.
    FeedbackModal {
        agent_id: AgentId,
        modal_id: crate::views::feedback_modal::FeedbackModalId,
        composition_id: crate::views::feedback_modal::FeedbackCompositionId,
    },
}
impl ClipboardPasteTarget {
    /// Telemetry surface label for the empty-clipboard paste-key event.
    pub fn surface_str(&self) -> &'static str {
        match self {
            Self::AgentPrompt { .. } => "agent",
            Self::DashboardDispatch => "dashboard",
            Self::DashboardPeek { .. } => "peek",
            Self::FeedbackModal { .. } => "feedback_modal",
        }
    }
}
/// Result of the originating CLIPBOARD text read.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ClipboardTextRead {
    /// The text backend completed; `None` means it successfully found no text.
    Success(Option<String>),
    /// The text backend failed, so emptiness is unknown.
    Failed,
}
impl ClipboardTextRead {
    pub fn from_result<E>(result: Result<Option<String>, E>) -> Self {
        match result {
            Ok(text) => Self::Success(text),
            Err(_) => Self::Failed,
        }
    }
    pub fn as_deref(&self) -> Option<&str> {
        match self {
            Self::Success(text) => text.as_deref(),
            Self::Failed => None,
        }
    }
}
/// Result of inserting clipboard text into a target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardTextInsertion {
    /// The target accepted and applied the text operation.
    Inserted,
    /// The payload had no pasteable text.
    Empty,
    /// The target rejected a non-empty text operation.
    Failed,
}
#[derive(Debug, Clone)]
pub enum ClipboardPasteSource {
    /// Ctrl/Cmd+V: CLIPBOARD text was read before the attachment probe.
    ClipboardKey {
        text: ClipboardTextRead,
        tip_showing: bool,
    },
    /// Dashboard bracketed text waits for the attachment probe's image-wins decision.
    BracketedDeferred { text: String },
    /// Agent bracketed text was inserted synchronously before probing attachments.
    BracketedInserted {
        text: String,
        insertion: ClipboardTextInsertion,
    },
}
impl ClipboardPasteSource {
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::ClipboardKey { text, .. } => text.as_deref(),
            Self::BracketedDeferred { text } | Self::BracketedInserted { text, .. } => Some(text),
        }
    }
    pub fn is_clipboard_key(&self) -> bool {
        matches!(self, Self::ClipboardKey { .. })
    }
    pub fn is_bracketed(&self) -> bool {
        !self.is_clipboard_key()
    }
    pub fn tip_showing(&self) -> bool {
        matches!(
            self,
            Self::ClipboardKey {
                tip_showing: true,
                ..
            }
        )
    }
    pub fn text_read_failed(&self) -> bool {
        matches!(
            self,
            Self::ClipboardKey {
                text: ClipboardTextRead::Failed,
                ..
            }
        )
    }
    /// Non-blank text to insert when the probe attached nothing; `None` for bracketed text the target already inserted.
    pub fn text_to_insert_on_miss(&self, image: &ProbedAttachment) -> Option<&str> {
        if !matches!(
            image,
            ProbedAttachment::NoRaster
                | ProbedAttachment::ProbeDropped
                | ProbedAttachment::ProbeFailed
        ) {
            return None;
        }
        let text = match self {
            Self::ClipboardKey { text, .. } => text.as_deref(),
            Self::BracketedDeferred { text } => Some(text.as_str()),
            Self::BracketedInserted { .. } => None,
        };
        text.filter(|text| !text.trim().is_empty())
    }
    pub fn synchronous_insertion(&self) -> Option<ClipboardTextInsertion> {
        match self {
            Self::BracketedInserted { insertion, .. } => Some(*insertion),
            _ => None,
        }
    }
}
/// Completion context carried through one deferred clipboard-attachment paste.
#[derive(Debug, Clone)]
pub struct ClipboardPasteContext {
    pub target: ClipboardPasteTarget,
    pub source: ClipboardPasteSource,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardPasteFailure {
    /// CLIPBOARD text could not be read.
    TextRead,
    /// Image/file attachment probing failed.
    AttachmentRead,
    /// The target rejected text without showing its own message.
    TargetInsertion,
    /// The target already showed a persistence/insertion message.
    AlreadyReported,
}
/// Result after the originating target applies a clipboard probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardPasteCompletion {
    /// Text, image, or file URL reached the target.
    Handled,
    /// All local reads completed successfully with no payload.
    FullMiss,
    /// The result was intentionally discarded because its target or baseline went stale.
    Dropped,
    /// The probe, persistence, or target insertion failed.
    Failed(ClipboardPasteFailure),
}
pub fn reduce_clipboard_paste_completion(
    source: &ClipboardPasteSource,
    attachment: ClipboardPasteCompletion,
    file: Option<ClipboardPasteCompletion>,
    text: Option<ClipboardTextInsertion>,
) -> ClipboardPasteCompletion {
    match attachment {
        ClipboardPasteCompletion::FullMiss => {}
        other => return other,
    }
    if let Some(file) = file {
        return file;
    }
    if source.text_read_failed() {
        return ClipboardPasteCompletion::Failed(ClipboardPasteFailure::TextRead);
    }
    if let Some(insertion) = text.or(source.synchronous_insertion()) {
        return match insertion {
            ClipboardTextInsertion::Inserted => ClipboardPasteCompletion::Handled,
            ClipboardTextInsertion::Empty => ClipboardPasteCompletion::FullMiss,
            ClipboardTextInsertion::Failed => {
                ClipboardPasteCompletion::Failed(ClipboardPasteFailure::TargetInsertion)
            }
        };
    }
    ClipboardPasteCompletion::FullMiss
}
/// Outcome of the off-thread image portion of a clipboard attachment probe.
#[derive(Debug)]
pub enum ProbedAttachment {
    /// Decoded (and, for the agent, persisted) image ready to insert.
    Image(crate::prompt_images::PastedImage),
    /// The session persist failed; the completion shows a toast.
    PersistFailed(String),
    /// The pasteboard probe completed normally without raster data.
    NoRaster,
    /// Discarded because the pasteboard changed under the paste or the bracketed payload did not match; the reason is telemetry's only.
    ProbeDropped,
    /// The attachment probe task failed or timed out.
    ProbeFailed,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorFixTarget {
    pub agent_id: AgentId,
    pub session_id: Option<acp::SessionId>,
    pub session_binding_epoch: u32,
    pub cwd: std::path::PathBuf,
}
/// Aftermath of a successful session delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterSessionDelete {
    /// Picker delete: stay put.
    Stay,
    /// `/delete` from a standalone agent: return to welcome.
    Welcome,
    /// `/delete` from a dashboard-attached agent, or dashboard row delete.
    Dashboard,
    /// Unused optimistic home husk: remove on-disk data without a toast or view change.
    UnusedHusk,
}
/// Async side effect produced by [`super::dispatch::dispatch`].
/// The event loop spawns these into a `JoinSet`; completions come back through [`TaskResult`] as `Action::TaskComplete`.
#[derive(Debug)]
pub enum Effect {
    LoginProvider {
        provider: LoginProvider,
        attempt_id: u64,
        cancel: tokio_util::sync::CancellationToken,
    },
    LogoutProvider(LoginProvider),
    /// Run a `command` status line.
    RunStatusLineCommand(StatusLineRun),
    /// Create a new ACP session.
    CreateSession {
        agent_id: AgentId,
        cwd: std::path::PathBuf,
        /// When set, injected as `_meta.modelId` into the `NewSessionRequest`.
        /// The shell then spawns the session with the correct model and agent type from the start.
        /// Avoids a follow-up `SetSessionModel` roundtrip.
        model_id: Option<acp::ModelId>,
        /// Per-create permission mode.
        /// Dashboard dispatches set this so their staged mode overrides process-global defaults without persisting.
        permission_mode_override: Option<PermissionModeKind>,
        /// Client-chosen session ID (`--session-id` / `meta.sessionId`).
        preferred_session_id: Option<String>,
        /// Gateway light-frontend for **this** session only (`/chat` one-shot or CLI `--chat` via `SessionFlags.chat_mode`).
        /// Does not sticky-set process-wide mode.
        chat_kind: bool,
    },
    /// Change the process working directory (dashboard location picker, `/cd`).
    SetWorkingDir {
        path: std::path::PathBuf,
    },
    /// Create a git worktree and then create or load an ACP session in it.
    /// When `load_session_id` is `Some`, loads that session in the new worktree instead of creating a fresh one (`--resume` with `--worktree`).
    CreateWorktreeSession {
        agent_id: AgentId,
        load_session_id: Option<String>,
        label: Option<String>,
        /// Optional branch/tag/commit to base the worktree on (CLI `--ref`).
        git_ref: Option<String>,
        /// Staged dashboard `/model` selection, injected as `_meta.modelId` so the worktree's `NewSessionRequest` spawns with the right model.
        /// Mirrors [`Effect::CreateSession::model_id`]. `None` for the welcome / CLI / fork paths.
        model_id: Option<acp::ModelId>,
        /// Per-create permission mode for a fresh worktree session. Ignored when resuming an existing session.
        permission_mode_override: Option<PermissionModeKind>,
        /// Explicit `--session-id`: names the worktree checkout and is the `meta.sessionId` on create.
        preferred_session_id: Option<String>,
        /// Pager-minted `meta.sessionId` for a fresh worktree create without an explicit id, so its setup phases route; not the checkout name.
        minted_session_id: Option<String>,
        /// One-shot `/chat` or sticky `--chat`: stamp `_meta` kind=chat on fresh create (resume uses `LoadSession.chat_kind` instead).
        chat_kind: bool,
    },
    /// Load (resume) an existing ACP session by ID.
    /// `session_cwd` overrides the CWD sent in the `LoadSessionRequest`.
    /// Strict load: does not create if missing.
    LoadSession {
        agent_id: AgentId,
        session_id: String,
        session_cwd: Option<std::path::PathBuf>,
        /// Conversation-entry bit (`source == "conversation"`), not sticky `--chat`.
        chat_kind: bool,
    },
    /// Scan enabled foreign session stores without delaying the native list.
    ScanForeignSessions {
        cwd: std::path::PathBuf,
        compat: distill_foreign_sessions::EnabledForeignSessionSources,
        distill_home: std::path::PathBuf,
        coordinator: crate::app::ForeignScanCoordinator,
        seq: u64,
    },
    /// Canonicalize the launch cwd off the event-loop thread before store access.
    CanonicalizeForeignResumeCwd {
        requested_cwd: std::path::PathBuf,
        launch_token: u64,
    },
    /// Detect the newest resumable foreign session without delaying first paint.
    DetectForeignResumeHint {
        canonical_cwd: std::path::PathBuf,
        compat: distill_foreign_sessions::EnabledForeignSessionSources,
        distill_home: std::path::PathBuf,
        launch_token: u64,
    },
    /// Fetch a picker session list.
    FetchSessionList {
        /// The picker this fetch was issued for; the result routes back to this host's storage only.
        host: crate::views::session_picker_surface::SessionPickerHost,
        cwd_override: Option<std::path::PathBuf>,
        /// Live generation of the requesting picker at dispatch time.
        generation: u64,
        /// Text search pushed down to `x.ai/session/list` as `query` (chat mode: forwarded to the backend conversations search).
        /// `None` fetches the unfiltered list.
        query: Option<String>,
        /// Snapshot of [`crate::app::app_view::AppView::session_picker_list_seq`].
        /// The response is dropped when no longer current, so out-of-order completions can't clobber newer results.
        seq: u64,
        /// Optional unified-list `kind` facet filter (`"chat"` / `"build"`).
        /// When set, stamped as `_meta["x.ai/facetFilters"].kind`.
        /// The shell then honors multi-source history under `--chat` instead of forcing chat-only.
        kind_filter: Option<Vec<String>>,
        /// Server-side `session_kind=headless` policy: `Only` while the picker is on the Headless page, `Exclude` everywhere else.
        /// Applied by the shell before its page truncation, so it cannot be a client refilter.
        headless_policy: distill_shell::session::unified_list::HeadlessPolicy,
    },
    /// Coalesce picker search keystrokes: fires [`TaskResult::SessionSearchDebounceExpired`] after a short sleep.
    /// The expiry acts only if `host`, `generation`, and `seq` still name the live picker.
    /// Build runs an FTS5 deep search against the deep-search seq; chat refetches from the server against the list seq.
    DebounceSessionSearch {
        host: crate::views::session_picker_surface::SessionPickerHost,
        generation: u64,
        query: String,
        seq: u64,
    },
    /// Fetch the leader session roster (FleetView dashboard) via `x.ai/sessions/list`.
    /// Only issued in leader mode while the dashboard is open.
    FetchRoster,
    /// Fetch the local on-disk session list (dormant/idle sessions) for the dashboard via `x.ai/session/list`.
    /// This is the non-leader fallback for the FleetView roster.
    /// Issued while the dashboard is open and NOT in leader mode so the dashboard shows idle sessions instead of being empty.
    FetchDashboardSessions,
    /// Lazily open dashboard v2's process-owned SQLite store and read its initial snapshot off the event-loop thread.
    LoadWorkspaceSnapshot {
        db_path: std::path::PathBuf,
    },
    /// Apply one mutation; [`TaskResult::WorkspaceWriteCompleted`] carries the handle and mutation back.
    WriteWorkspace {
        store: distill_dashboard_store::WorkspaceStore,
        mutation: WorkspaceMutation,
    },
    /// Reads a snapshot only when SQLite's per-connection data version changed.
    RefreshWorkspace {
        store: distill_dashboard_store::WorkspaceStore,
        known_data_version: i64,
    },
    /// Load card detail for a specific session (lazy, reads chat history from disk).
    LoadCardDetail {
        /// The picker whose row expansion requested the detail.
        host: crate::views::session_picker_surface::SessionPickerHost,
        /// Live generation of the requesting picker at dispatch time.
        generation: u64,
        source: String,
        session_id: String,
        cwd: String,
        /// Snapshot of the requesting picker's detail seq; the result is dropped when the picker's rows or filters changed meanwhile.
        seq: u64,
    },
    /// Restore a remote session from GCS then load it.
    /// Only Build rows reach this effect: conversation rows have no GCS archive.
    RestoreAndLoadSession {
        agent_id: AgentId,
        session_id: String,
        session_cwd: String,
    },
    /// Send a prompt to the agent.
    SendPrompt {
        agent_id: AgentId,
        session_id: acp::SessionId,
        text: String,
        /// Client-generated UUID echoed back on every notification and the PromptResponse so the client can correlate them to this prompt.
        prompt_id: String,
        /// Recognized slash-token byte ranges into `text`.
        /// Stamped into the content block `_meta` (`skillTokenRanges`) when non-empty so replay restyles the echo like the composer did.
        /// Contract: the offsets index the block's `text` displayed verbatim, never combined with a `displayText` override.
        skill_token_ranges: Vec<std::ops::Range<usize>>,
    },
    /// `session/prompt` with `_meta.executePlan` after a post-turn plan approve
    ExecutePlan {
        agent_id: AgentId,
        session_id: acp::SessionId,
        prompt_id: String,
        plan_file_content: String,
        /// File URI of the keep. Omitted on the wire when empty.
        plan_file_uri: Option<String>,
    },
    /// Send a direct bash command to the agent (with typed PromptBlockMeta).
    SendBashCommand {
        agent_id: AgentId,
        session_id: acp::SessionId,
        command: String,
        /// See [`Effect::SendPrompt::prompt_id`].
        prompt_id: String,
    },
    /// Cancel the current turn.
    CancelTurn {
        session_id: acp::SessionId,
        cancel_subagents: bool,
        /// What user gesture triggered the cancel (ESC / Ctrl+C / mouse).
        /// Sent on `session/cancel` as `_meta.cancelTrigger` so the agent's `mid_turn_abort` telemetry can distinguish them.
        /// `None` for programmatic cancels (login/reauth flows).
        trigger: Option<CancelTrigger>,
        /// `_meta.rewindIfNoOutput` and `_meta.promptId` of the locally rewound turn.
        /// Set only when the pager restored the prompt into the composer.
        rewind_prompt_id: Option<String>,
    },
    /// Run a manual `/compact` command.
    Compact {
        agent_id: AgentId,
        session_id: acp::SessionId,
        user_context: Option<String>,
    },
    /// Kill a background task.
    KillBgTask {
        session_id: acp::SessionId,
        task_id: String,
        source: distill_shell::extensions::task::TaskKillSource,
    },
    /// Cancel a subagent via `x.ai/subagent/cancel`.
    KillSubagent {
        session_id: acp::SessionId,
        subagent_id: String,
        attempt_id: Option<String>,
    },
    DeleteScheduledTask {
        session_id: acp::SessionId,
        task_id: String,
    },
    /// Demote a foreground execute tool to background.
    DemoteToBackground {
        session_id: acp::SessionId,
        tool_call_id: String,
    },
    /// Switch active model.
    SwitchModel {
        agent_id: AgentId,
        session_id: acp::SessionId,
        model_id: acp::ModelId,
        effort: Option<ReasoningEffort>,
        /// The model that was active before the optimistic UI update in `set_default_model`.
        /// `None` for `Action::SwitchModel` (no optimistic update).
        /// Threaded through to `SwitchModelComplete` so `IncompatibleAgent` can roll back.
        prev_model_id: Option<acp::ModelId>,
    },
    /// Turn auto effort on for the session: the harness picks the effort for
    /// every model call. Fire-and-forget; the optimistic flag is set at dispatch.
    SetEffortAuto {
        agent_id: AgentId,
        session_id: acp::SessionId,
        model_id: acp::ModelId,
    },
    /// Fetch changelog from CDN (both markdown and structured JSON).
    /// Runs off the render path via `spawn_blocking`.
    /// Result is cached on `AppView` so `/release-notes` and the welcome screen share it.
    FetchChangelog,
    /// Persist the hidden announcement ids to disk.
    PersistAnnouncementsHidden {
        hidden_ids: std::collections::BTreeSet<String>,
    },
    /// Persist `[privacy].privacy_banner_acked` (RFC 3339 dismiss time).
    PersistPrivacyBannerAcked {
        acked_at: String,
    },
    /// Persist a plugin CTA dismissal to `[plugin_cta].dismissed`; the locked write sleep-polls the config-init flock, so it must run off the render path.
    /// config-init flock, so it must run off the render path.
    PersistPluginCtaDismissed {
        plugin_id: String,
    },
    /// Persist the consent answer to `[consent]` in config.toml.
    PersistConsentAnswer {
        account: Option<String>,
        notice_id: String,
        version: i32,
        acked: bool,
    },
    /// Files the acceptance server side; the local marker is what stops the re-prompt if it fails.
    RecordConsentUpstream {
        notice_id: String,
        version: i32,
    },
    /// Persist memory modal fullscreen preference to `[hints]` in config.toml.
    PersistMemoryFullscreen {
        fullscreen: bool,
    },
    /// Persist the dashboard's `[dashboard]` configuration to `~/.grok/config.toml`.
    /// Multi-pager safe via `config_toml_edit::read_config_document_for_edit`, which loads, modifies, then writes the whole document.
    /// Concurrent pagers may produce last-writer-wins behaviour but never corrupt the file.
    PersistDashboard(crate::views::dashboard::PersistedDashboard),
    /// Persist a per-command worktree mode preference to `[hints]` in config.toml.
    /// `config_key` is the TOML key under `[hints]` (`"new_session_worktree_mode"` or `"fork_worktree_mode"`).
    PersistWorktreeMode {
        mode: crate::app::app_view::WorktreeMode,
        config_key: &'static str,
    },
    /// Persist preferred model (and effort if Some) to config.toml.
    PersistPreferredModel {
        model_id: acp::ModelId,
        reasoning_effort: Option<ReasoningEffort>,
    },
    /// Persist the permission mode to config.toml and notify the agent via ACP.
    /// See [`PermissionModePersist`] for rollback behavior.
    PersistPermissionMode {
        /// One of `"ask"`, `"always-approve"`, or `"default"`.
        canonical: &'static str,
        session_id: Option<acp::SessionId>,
        persist: PermissionModePersist,
    },
    /// Persist the utility model and effort together, then publish the selection.
    PersistUtilityModel {
        model: String,
        effort: Option<ReasoningEffort>,
    },
    /// Persist a typed setting; roll back the cache if the write fails.
    PersistSetting {
        key: crate::settings::SettingKey,
        value: crate::settings::SettingValue,
        rollback_value: crate::settings::SettingValue,
    },
    /// Toggle mouse reporting off and on to unwedge xterm.js's button tracker
    /// (see `AgentView::reset_wedged_mouse_reporting`). An effect so it rides the escape
    /// writer; `process_effects` re-checks capture so a toggle-off in the same batch wins.
    ResetMouseReporting,
    /// Send structured prompt blocks to the agent.
    /// Used for skill injection where the prompt consists of multiple content blocks (metadata plus skill body).
    SendPromptBlocks {
        agent_id: AgentId,
        session_id: acp::SessionId,
        blocks: Vec<acp::ContentBlock>,
        /// See [`Effect::SendPrompt::prompt_id`].
        prompt_id: String,
    },
    /// Cancel-and-send: `session/prompt` stamped with `_meta.sendNow`, so the shell cancels the running turn and runs this prompt next.
    /// Background tasks and the rest of the queue survive.
    /// Carries structured blocks so pasted images ride along.
    SendPromptNow {
        agent_id: AgentId,
        session_id: acp::SessionId,
        blocks: Vec<acp::ContentBlock>,
        /// See [`Effect::SendPrompt::prompt_id`].
        prompt_id: String,
    },
    /// Toggle plan mode: fire-and-forget signal to the shell.
    TogglePlanMode {
        session_id: acp::SessionId,
    },
    /// Remove a server-owned queued prompt: fire-and-forget `x.ai/queue/remove`.
    /// The agent re-broadcasts the authoritative queue.
    QueueRemove {
        session_id: acp::SessionId,
        id: String,
        expected_version: u64,
    },
    /// Reorder server-owned queued prompts: fire-and-forget `x.ai/queue/reorder`.
    QueueReorder {
        session_id: acp::SessionId,
        ordered_ids: Vec<String>,
    },
    /// Clear the caller's server-owned queued prompts: fire-and-forget `x.ai/queue/clear`.
    QueueClear {
        session_id: acp::SessionId,
    },
    /// Replace the text of a server-owned queued prompt in place: fire-and-forget `x.ai/queue/edit`.
    /// The session actor's serialized mailbox makes this last-writer-wins for concurrent edits.
    /// The rebroadcast of `x.ai/queue/changed` is the truth signal.
    QueueEdit {
        session_id: acp::SessionId,
        id: String,
        new_text: String,
    },
    /// Hold a server-owned row out of combine-on-promote while the composer edits it: fire-and-forget `x.ai/queue/hold_edit`.
    QueueHoldEdit {
        session_id: acp::SessionId,
        id: String,
    },
    /// Release a previous [`Self::QueueHoldEdit`]: `x.ai/queue/release_edit`.
    QueueReleaseEdit {
        session_id: acp::SessionId,
        id: String,
    },
    /// The same single version check applies, so a stale version no-ops the edit too.
    /// If the turn already ended, the agent saves a version-matching `new_text` to the row as an LWW edit instead.
    /// The edit survives and drains; it is never silently lost.
    QueueInterject {
        session_id: acp::SessionId,
        id: String,
        expected_version: u64,
        new_text: Option<String>,
    },
    /// Set the session mode via ACP `session/set_mode`.
    SetSessionMode {
        session_id: acp::SessionId,
        mode_id: acp::SessionModeId,
    },
    /// Set session mode then send a prompt, sequentially in one task.
    /// Used by `/plan <desc>` to guarantee the mode switch ACP call completes before the prompt is dispatched.
    SetModeThenPrompt {
        session_id: acp::SessionId,
        mode_id: acp::SessionModeId,
        agent_id: AgentId,
        text: String,
        prompt_id: String,
        /// See [`Effect::SendPrompt::skill_token_ranges`].
        skill_token_ranges: Vec<std::ops::Range<usize>>,
    },
    /// Fetch prompt history for the current session from the ACP agent.
    /// `session_id` scopes the per-CWD history file to this session (the agent's `filter_session_id` param).
    /// Up-arrow recall and the `/history` panel thus show only the current session's prompts.
    FetchPromptHistory {
        agent_id: AgentId,
        cwd: std::path::PathBuf,
        session_id: String,
    },
    /// Resolve the running agent name for a session (`x.ai/session/info`).
    FetchSessionAgentName {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Send AuthenticateRequest to the agent.
    Authenticate {
        request_seq: u64,
        method_id: acp::AuthMethodId,
        use_oauth: bool,
        force_interactive: bool,
    },
    /// Poll for auth URL from the agent (ext request).
    PollAuthUrl {
        request_seq: u64,
    },
    /// Submit a manually-pasted auth code (ext request).
    SubmitAuthCode {
        request_seq: u64,
        code: String,
    },
    /// Fetch MCP server list from the shell (x.ai/mcp/list).
    FetchMcpsList {
        agent_id: AgentId,
        session_id: acp::SessionId,
        cache: bool,
    },
    /// Trigger MCP OAuth for a server (x.ai/mcp/auth_trigger).
    McpAuthTrigger {
        agent_id: AgentId,
        session_id: acp::SessionId,
        server_name: String,
    },
    McpSetupSubmit {
        agent_id: AgentId,
        session_id: acp::SessionId,
        server_name: String,
        values: std::collections::HashMap<String, String>,
    },
    /// Fetch hooks list from the shell (x.ai/hooks/list).
    FetchHooksList {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Fetch plugins list from the shell (x.ai/plugins/list).
    FetchPluginsList {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Fetch the `/memory` modal contents (x.ai/memory/list).
    FetchMemoryList {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Turn memory on or off (x.ai/memory/toggle).
    MemoryToggle {
        agent_id: AgentId,
        session_id: acp::SessionId,
        enabled: bool,
    },
    /// Delete one memory note from the `/memory` modal (x.ai/memory/forget).
    MemoryForget {
        agent_id: AgentId,
        session_id: acp::SessionId,
        path: String,
        expected_content_hash: String,
    },
    /// Run `/flush` (x.ai/memory/flush) as a tracked agent command.
    MemoryFlush {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Run `/dream` (x.ai/memory/dream) as a tracked agent command.
    MemoryDream {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Execute a hooks management action via ACP.
    HooksAction {
        agent_id: AgentId,
        session_id: acp::SessionId,
        action: distill_hooks_plugins_types::HooksAction,
    },
    /// Execute a plugins management action via ACP.
    PluginsAction {
        agent_id: AgentId,
        session_id: acp::SessionId,
        action: distill_hooks_plugins_types::PluginsAction,
    },
    /// Fetch marketplace plugin list from the shell.
    FetchMarketplaceList {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Background check and auto-update for marketplace plugins on session start.
    CheckMarketplaceUpdates {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Fetch the official-marketplace catalog into agent-level CTA state, independent of the Extensions modal.
    FetchPluginCtaCatalog {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Fetch skills list from the shell (x.ai/skills/list).
    FetchSkillsList {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    FetchWorkflowsList {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Toggle a skill via x.ai/skills/toggle (enable/disable without restart).
    ToggleSkill {
        agent_id: AgentId,
        session_id: acp::SessionId,
        skill_name: String,
        enabled: bool,
    },
    /// Execute a marketplace action (install/uninstall/refresh) via ACP.
    MarketplaceAction {
        agent_id: AgentId,
        session_id: acp::SessionId,
        action: distill_hooks_plugins_types::MarketplaceAction,
    },
    /// Install a plugin from the inline CTA via `x.ai/marketplace/action`, reported back via `TaskResult::CtaPluginInstallDone`.
    InstallPluginFromCta {
        agent_id: AgentId,
        session_id: acp::SessionId,
        source_url_or_path: String,
        plugin_relative_path: String,
    },
    /// Reload plugins after a CTA install via `x.ai/plugins/action` (`PluginsAction::Reload`), reported back via `TaskResult::CtaPluginReloadDone`.
    /// Modal-independent.
    ReloadPluginsForCta {
        agent_id: AgentId,
        session_id: acp::SessionId,
        plugin_name: String,
    },
    /// Read the MCP server list after a CTA install via `x.ai/mcp/list`, reported back via `TaskResult::PluginCtaMcpsLoaded`.
    /// Modal-independent.
    FetchPluginCtaMcps {
        agent_id: AgentId,
        session_id: acp::SessionId,
        plugin_name: String,
    },
    /// Re-probe the MCP server list after a short delay while waiting for a just-installed plugin's servers to finish initializing.
    /// Sleeps, then runs the same `x.ai/mcp/list` fetch as `FetchPluginCtaMcps`, reported back via `TaskResult::PluginCtaMcpsLoaded`.
    RetryPluginCtaMcps {
        agent_id: AgentId,
        session_id: acp::SessionId,
        plugin_name: String,
    },
    /// Auto-dismiss the CTA's brief "installed" confirmation after a delay, reported back via `TaskResult::CtaInstalledDismissTimeout`.
    DismissCtaInstalled {
        agent_id: AgentId,
        plugin_name: String,
    },
    /// Upsert an MCP server via x.ai/mcp/upsert.
    UpsertMcpServer {
        agent_id: AgentId,
        session_id: acp::SessionId,
        name: String,
        config: Box<distill_shell::util::config::McpServerConfig>,
    },
    /// Delete an MCP server via x.ai/mcp/delete.
    DeleteMcpServer {
        agent_id: AgentId,
        session_id: acp::SessionId,
        server_name: String,
    },
    /// Live-toggle an MCP server via x.ai/mcp/toggle (no restart needed).
    ToggleMcpServer {
        agent_id: AgentId,
        session_id: acp::SessionId,
        server_name: String,
        enabled: bool,
    },
    /// Toggle a single MCP tool via x.ai/mcp/toggle_tool.
    ToggleMcpTool {
        agent_id: AgentId,
        session_id: acp::SessionId,
        server_name: String,
        tool_name: String,
        enabled: bool,
    },
    /// Share the current session via URL.
    ShareSession {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Fetch and display session info via x.ai/session/info.
    /// Auth lines are derived in the effect from SessionFlags and env (not Effect fields).
    ShowSessionInfo {
        agent_id: AgentId,
        session_id: acp::SessionId,
        show_resolved_model: bool,
        /// Usage-modal fetch generation; echoed back on the task result.
        nonce: u64,
    },
    /// Fetch and display detailed context usage via x.ai/session/info.
    ShowContextInfo {
        agent_id: AgentId,
        session_id: acp::SessionId,
        /// Usage-modal fetch generation; echoed back on the task result.
        nonce: u64,
    },
    /// Fetch current bundle cache status via `x.ai/bundle/status`.
    FetchBundleStatus,
    /// Fetch a bundled entry's raw content via `x.ai/bundle/entry/get`.
    FetchCatalogEntry {
        kind: String,
        name: String,
    },
    /// Send feedback about the current session (fire-and-forget POST).
    /// `origin` rides through to the completion so a modal send's parked consent can be matched or dropped.
    SendFeedback {
        agent_id: AgentId,
        session_id: acp::SessionId,
        feedback_text: String,
        images: Vec<distill_shell::session::FeedbackImage>,
        metadata: Option<serde_json::Value>,
        /// Ask the shell to mint a one-shot upload capability after this report succeeds.
        request_trace_upload_token: bool,
        draft: Option<DraftFeedbackBody>,
        origin: FeedbackSendOrigin,
    },
    FeedbackDraftRequest {
        agent_id: AgentId,
        session_id: acp::SessionId,
        request: crate::views::feedback_modal::FeedbackDraftRequest,
    },
    /// One-shot session archive for a feedback report (after the text POST).
    /// Emitted only from the matching successful feedback-POST reduction (modal path)
    /// or the legacy card's persisted-consent path, never as a sibling of the POST.
    UploadFeedbackTrace {
        agent_id: AgentId,
        session_id: acp::SessionId,
        /// `Some` for a modal one-shot: correlates the completion with the exact POST attempt that earned it.
        submission_id: Option<crate::views::feedback_modal::FeedbackSubmissionId>,
        /// Typed intent for a modal one-shot; legacy persisted-consent uploads carry `None`.
        intent: Option<crate::views::feedback_modal::FeedbackTraceUploadIntent>,
        /// Capability minted by the successful feedback POST; absent on the legacy persisted-consent path.
        trace_upload_token: Option<String>,
    },
    /// Save a remember note to the active mode's global memory storage.
    SaveMemoryNote {
        agent_id: AgentId,
        text: String,
        cwd: std::path::PathBuf,
        /// `Some` for an active session; `None` only for the pre-session fallback.
        pinned_mode: Option<distill_shell::config::MemoryMode>,
    },
    /// Send raw note to x.ai/memory/rewrite for LLM-powered reformatting.
    /// On success, the rewritten text populates the prompt for inline review.
    /// On failure, falls back to showing the raw text for review.
    RewriteMemoryNote {
        agent_id: AgentId,
        session_id: acp::SessionId,
        raw_text: String,
        context_summary: String,
        /// Monotonic nonce correlating this request with the modal that opened it, so stale results don't populate a different review.
        nonce: u64,
    },
    /// Re-fetch available commands (including skills) from the shell.
    /// Sent after `SessionCreated` / `WorktreeSessionCreated` to work around a race.
    /// The shell's `AvailableCommandsUpdate` notification can arrive before the pager has set `session_id`, and is then silently dropped.
    RefreshAvailableCommands {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    /// Fire a /btw side question via x.ai/btw ext method.
    SendBtw {
        agent_id: AgentId,
        session_id: acp::SessionId,
        question: String,
        /// Composer images. Encoded off the TUI thread. Empty keeps the text-only wire.
        images: Vec<crate::prompt_images::PastedImage>,
        cwd: std::path::PathBuf,
        /// Correlates minimal responses; fullscreen leaves this unset.
        minimal_request_id: Option<uuid::Uuid>,
    },
    /// Request a session recap via the x.ai/recap ext method.
    /// Fire-and-forget: the recap arrives later as a `SessionRecap` notification.
    SendRecap {
        session_id: acp::SessionId,
        auto: bool,
    },
    /// Send a mid-turn interjection via x.ai/interject ext method.
    SendInterject {
        agent_id: AgentId,
        session_id: acp::SessionId,
        text: String,
        /// Client-minted id echoed back on the `x.ai/session/interjection` broadcast so the originator can dedup its optimistic local block.
        interjection_id: String,
        /// Structured text and image content blocks.
        /// `None` for text-only interjections; the wire shape stays byte-identical to legacy.
        blocks: Option<Vec<acp::ContentBlock>>,
    },
    /// Log out via `x.ai/auth/logout` (shell clears auth.json and in-memory state).
    Logout,
    /// Cancel an in-flight interactive auth on the shell (`x.ai/auth/cancel`).
    /// Used when the user abandons mid-session `/login` so the device-code poll stops instead of running until the code expires.
    /// `request_seq` scopes the cancel so a delayed RPC cannot tear down a successor login.
    CancelAuth {
        request_seq: u64,
    },
    /// Re-check subscription status via `x.ai/auth/check_subscription`.
    /// `verify` scopes the result to a deferred-gate verification (see [`crate::app::subscription`]); `None` for generic checks.
    CheckSubscription {
        verify: Option<u64>,
    },
    /// One-shot subscription re-check triggered by a credit-limit 403.
    /// If the tier changed, the stashed prompt is retried instead of showing the upsell modal.
    CreditLimitRecheck {
        agent_id: AgentId,
    },
    /// Schedule a 5s timer that fires `TaskResult::PaywallCheckTick`.
    SchedulePaywallCheck,
    /// Schedule `TaskResult::GateVerifyTimeout { generation }` after [`crate::app::subscription::GATE_VERIFY_TIMEOUT`].
    /// [`crate::app::subscription::GATE_VERIFY_TIMEOUT`].
    ScheduleGateVerifyTimeout {
        generation: u64,
    },
    /// Log out then authenticate sequentially in one task.
    SwitchAccount {
        request_seq: u64,
        method_id: acp::AuthMethodId,
        use_oauth: bool,
    },
    /// Clear the auth copy feedback after a delay if its generation is still current.
    ScheduleClearAuthCopyFeedback {
        generation: u64,
    },
    /// Register the current session in the active-sessions crash-recovery
    /// registry (`~/.grok/active_sessions.json`).
    RegisterActiveSession {
        session_id: acp::SessionId,
        cwd: String,
    },
    /// Unregister a session from the active-sessions registry (clean exit).
    UnregisterActiveSession {
        session_id: acp::SessionId,
    },
    /// Quit the application.
    Quit,
    /// Toggle coding data sharing via ACP.
    SetCodingDataSharing {
        agent_id: AgentId,
        opted_in: bool,
        /// Write generation, echoed back on the `TaskResult`.
        /// Writes to this endpoint are concurrent, so only the newest result sets the mirror; an older success only updates the pending write's rollback.
        seq: u64,
    },
    /// Rename the current session.
    RenameSession {
        agent_id: AgentId,
        session_id: acp::SessionId,
        title: String,
        cwd: std::path::PathBuf,
        kind: SessionKind,
    },
    /// Unpin the current session title (`/rename --auto`).
    ResetSessionTitle {
        agent_id: AgentId,
        session_id: acp::SessionId,
        cwd: std::path::PathBuf,
        kind: SessionKind,
        /// Pre-clear caches so `ResetSessionTitleFailed` can restore the pin.
        previous_display_name: Option<String>,
        previous_generated_title: Option<String>,
    },
    /// Delete a session's stored data (local and remote) via `x.ai/session/delete`.
    DeleteSession {
        source: String,
        session_id: String,
        cwd: String,
        after: AfterSessionDelete,
    },
    /// Deep-search sessions by content (FTS via ACP).
    DeepSearchSessions {
        /// The picker this search was issued for; the result routes back to this host's storage only.
        host: crate::views::session_picker_surface::SessionPickerHost,
        /// Live generation of the requesting picker at dispatch time.
        generation: u64,
        query: String,
        seq: u64,
        /// Server-side headless policy of the page that consumes the hits: `Only` on the Headless page, `Exclude` everywhere else.
        /// Unresolved index rows are omitted from both classified views.
        headless_policy: distill_shell::session::unified_list::HeadlessPolicy,
    },
    /// Call `x.ai/session/fork` to create a peer session that resumes from `parent_session_id` in the same cwd (no worktree).
    /// Mirror of the worktree branch of [`Effect::CreateWorktreeSession`].
    /// The worktree-fork path reuses `CreateWorktreeSession { load_session_id }` directly so we get worktree creation and code restore for free.
    ForkSession {
        agent_id: AgentId,
        parent_session_id: acp::SessionId,
        parent_cwd: std::path::PathBuf,
        /// Whether the parent session lives in a git worktree.
        /// When `true`, the fork payload sets `sourceWorkspaceDir` so the shell preserves prompt-display provenance.
        parent_is_worktree: bool,
        /// Optional client-chosen ID for the forked session (`--session-id` with `--fork-session`).
        new_session_id: Option<String>,
    },
    /// Read session display fields from local `summary.json` after load/resume.
    /// Those are the title (and `/rename` manual-ness) plus the last-turn summary for the dashboard secondary line.
    HydrateSessionMetaFromDisk {
        agent_id: AgentId,
        session_id: acp::SessionId,
        cwd: std::path::PathBuf,
        /// [`crate::app::agent_view::AgentView::last_turn_summary_gen`] at enqueue.
        /// The disk result applies only when this still matches on completion.
        last_turn_summary_gen: u64,
    },
    FetchRewindPoints {
        agent_id: AgentId,
        session_id: acp::SessionId,
    },
    RewindExecute {
        agent_id: AgentId,
        session_id: acp::SessionId,
        target_prompt_index: usize,
    },
    /// Fetch billing/credit usage from the agent's `x.ai/billing` extension.
    /// When `silent` is true the result updates `credit_balance` without pushing a system message into scrollback.
    /// The silent form is used for automatic refreshes on session init and after each turn.
    FetchBilling {
        agent_id: AgentId,
        silent: bool,
        /// Usage-modal fetch generation (`0` means a background refresh; those never touch the modal's loading/error flags).
        nonce: u64,
    },
    /// Fetch billing data at the app level (no agent required).
    /// Used on startup to populate the welcome-screen credit warning, and by the dashboard's `/usage` modal.
    FetchAppBilling {
        /// Usage-modal fetch generation (`0` means a background refresh that settles no modal).
        nonce: u64,
    },
    /// Fetch ChatGPT account usage through the isolated Codex auth store.
    /// `agent_id` is used by minimal mode to route the rendered result into
    /// the originating scrollback; dashboard modal fetches use `None`.
    FetchChatGptUsage {
        agent_id: Option<AgentId>,
        nonce: u64,
    },
    /// Fetch OpenRouter per-key usage through its isolated credential store.
    /// `agent_id` is used by minimal mode to route the rendered result into
    /// the originating scrollback; dashboard modal fetches use `None`.
    FetchOpenRouterUsage {
        agent_id: Option<AgentId>,
        nonce: u64,
    },
    /// Fetch per-session token/cost via `x.ai/session/usage` (auth-agnostic).
    FetchSessionUsage {
        agent_id: AgentId,
        session_id: acp::SessionId,
        /// Usage-modal fetch generation; echoed back on the task result.
        nonce: u64,
    },
    /// Re-fetch remote settings to check subscription gate.
    RefreshGate,
    /// Spawn a debounce sleep task for shell suggestions.
    /// `agent_id` rides to the expiry so the fetch is built from the arming agent, not whatever view is active when the timer fires.
    DebounceSuggestions {
        agent_id: AgentId,
        generation: u64,
    },
    /// Spawn a debounce sleep task for plugin-CTA keyword matching.
    DebouncePluginCta {
        agent_id: AgentId,
        generation: u64,
    },
    /// Send an ACP `x.ai/suggest` request to the shell.
    /// `agent_id` is echoed on the result so the response routes to the agent that fetched, not whatever view is active when it lands.
    FetchShellSuggestions {
        agent_id: AgentId,
        text: String,
        cursor: usize,
        cwd: String,
        generation: u64,
        limit: usize,
        include_ai: bool,
        ai_model: Option<String>,
        session_id: Option<String>,
        /// Deterministic Tab fetches run only the shell's token providers (path/file); the as-you-type pipeline keeps all of them.
        token_only: bool,
    },
    /// Send an ACP `x.ai/suggestPrompt` request to the shell.
    /// It predicts the user's likely next prompt after a completed turn (tab autocomplete ghost text).
    FetchPromptSuggestion {
        agent_id: AgentId,
        generation: u64,
        /// Suggestion model resolved by the pager (`grok-4.6` when the catalog offers it).
        /// `None` makes the shell fall back to the session model.
        model: Option<String>,
        session_id: Option<String>,
    },
    /// Probe the clipboard for an attachment off the event-loop thread (osascript image/file-url read, image decode, session persist).
    /// The chip then attaches via [`TaskResult::ClipboardAttachmentProbed`].
    /// Keeps the paste handler from blocking the render thread on that I/O.
    ProbeClipboardAttachment {
        ctx: ClipboardPasteContext,
        /// Pasteboard `changeCount` at enqueue; the probe drops the attachment if it moved before or during the read. `None` leaves the read unguarded.
        change_count: Option<u64>,
    },
    /// Bounded disk read for an adopted feedback image. The original file remains until completion installs the bytes.
    RehydrateFeedbackImage {
        agent_id: AgentId,
        modal_id: crate::views::feedback_modal::FeedbackModalId,
        image_identity: u64,
        path: std::path::PathBuf,
    },
    /// Prepare terminal preview bytes off the event-loop thread.
    PreparePromptImagePreview {
        preparation: crate::prompt_images::PromptImagePreviewPreparation,
    },
    PlanDoctorFix {
        target: DoctorFixTarget,
        report: Box<crate::diagnostics::DiagnosticReport>,
        terminal: crate::terminal::TerminalContext,
        request: crate::slash::command::DoctorRequest,
    },
    ApplyDoctorFix {
        target: DoctorFixTarget,
        plan: Box<crate::diagnostics::FixPlan>,
    },
}
/// Wire params for `x.ai/session/rename`.
/// Shared with the effect executor so dispatch tests can pin the exact camelCase payload.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RenameSessionRequest {
    pub session_id: String,
    pub title: String,
    pub cwd: String,
    pub kind: SessionKind,
    /// An empty title with `true` is the unpin convention.
    /// Omitted when false so ordinary rename payloads stay byte-identical for old shells.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset_to_auto: bool,
}
impl RenameSessionRequest {
    pub(crate) fn for_rename(
        session_id: String,
        title: String,
        cwd: String,
        kind: SessionKind,
    ) -> Self {
        Self {
            session_id,
            title,
            cwd,
            kind,
            reset_to_auto: false,
        }
    }
    pub(crate) fn for_reset(session_id: String, cwd: String, kind: SessionKind) -> Self {
        Self {
            session_id,
            title: String::new(),
            cwd,
            kind,
            reset_to_auto: true,
        }
    }
}
/// Outcome of an `x.ai/subagent/cancel` request, telling dispatch whether the pager must finalize the subagent row itself.
#[derive(Debug)]
pub enum SubagentKillOutcome {
    /// Shell stopped a live subagent; a real `SubagentFinished` is coming.
    StoppedLive,
    /// Nothing live to stop (orphan / already finished), so no finish is coming and the pager finalizes the row.
    /// `status` carries the real terminal status for an already-finished orphan; `None` (unknown id / older shell) renders as "cancelled".
    NothingLive { status: Option<String> },
    /// The cancel RPC failed; the subagent may still be running, so leave the row alone rather than show a false terminal state.
    RpcFailed,
}
#[derive(Debug)]
pub enum McpAuthTriggerOutcome {
    Authenticated,
    SetupRequired(crate::views::mcps_modal::McpSetupConfig),
}
#[derive(Clone, Debug)]
pub enum DoctorPlanningOutcome {
    Listing(String),
    Plan(Box<crate::diagnostics::FixPlan>),
    RunLocally(String),
}
/// Result from a completed async [`Effect`].
///
/// Wrapped in `Action::TaskComplete` and dispatched synchronously.
impl TaskResult {
    /// True for results that deliver the first usable session.
    /// A quit before dispatch abandons instead of recording; accepted so the token stays single-owner.
    pub fn ends_startup(&self) -> bool {
        match self {
            TaskResult::SessionCreated { .. }
            | TaskResult::SessionLoaded { .. }
            | TaskResult::WorktreeSessionCreated { .. }
            | TaskResult::WorktreeForked { .. } => true,
            TaskResult::WithPinnedMemoryMode { result, .. } => result.ends_startup(),
            _ => false,
        }
    }
}
#[derive(Debug)]
pub enum WorkspaceMutation {
    Upsert(Vec<distill_dashboard_store::NewMember>),
    Remove(Vec<distill_dashboard_store::MemberKey>),
    Layout(distill_dashboard_store::LayoutPatch),
}
#[derive(Debug)]
pub struct WorkspaceMutationFailure {
    pub key: distill_dashboard_store::MemberKey,
    pub error: String,
    pub retryable: bool,
}
#[derive(Debug)]
pub enum WorkspaceWriteCompletion {
    Upsert {
        members: Vec<distill_dashboard_store::NewMember>,
        snapshot: Result<distill_dashboard_store::WorkspaceSnapshot, String>,
        failures: Vec<WorkspaceMutationFailure>,
    },
    Remove {
        keys: Vec<distill_dashboard_store::MemberKey>,
        snapshot: Result<distill_dashboard_store::WorkspaceSnapshot, String>,
        failures: Vec<WorkspaceMutationFailure>,
    },
    Layout {
        patch: distill_dashboard_store::LayoutPatch,
        outcome: distill_dashboard_store::LayoutApplyOutcome,
    },
}
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum TaskResult {
    ProviderLoginFinished {
        provider: LoginProvider,
        attempt_id: u64,
        result: Result<String, String>,
    },
    ProviderLogoutFinished {
        provider: LoginProvider,
        result: Result<(), String>,
    },
    ProviderLoginBrowserFallback {
        provider: LoginProvider,
        attempt_id: u64,
        url: String,
    },
    /// Session lifecycle result paired with the memory mode pinned by the
    /// actor that produced it. The wrapper lets all lifecycle variants share
    /// one typed metadata path.
    WithPinnedMemoryMode {
        agent_id: AgentId,
        memory_mode: Option<distill_shell::config::MemoryMode>,
        result: Box<TaskResult>,
    },
    /// A `command` status line finished.
    StatusLineCommandFinished {
        id: crate::app::status_line::RunId,
        outcome: crate::app::status_line::RunOutcome,
    },
    /// Session was created successfully.
    SessionCreated {
        agent_id: AgentId,
        session_id: acp::SessionId,
        models: Option<acp::SessionModelState>,
        modes: Option<acp::SessionModeState>,
    },
    /// Session creation failed.
    SessionFailed {
        agent_id: AgentId,
        error: String,
        /// The create RPC hit its bounded timeout, rather than failing early for another reason.
        timed_out: bool,
    },
    /// Worktree session was created successfully (worktree and ACP session).
    WorktreeSessionCreated {
        agent_id: AgentId,
        session_id: acp::SessionId,
        /// Root of the created worktree (for display).
        worktree_path: std::path::PathBuf,
        /// Effective cwd inside the worktree (preserves subdirectory offset).
        session_cwd: std::path::PathBuf,
        models: Option<acp::SessionModelState>,
        modes: Option<acp::SessionModeState>,
        strategy_summary: Option<String>,
    },
    /// Worktree created and session forked, but not yet loaded.
    /// The dispatch handler sets session_id eagerly, then emits LoadSession.
    WorktreeForked {
        agent_id: AgentId,
        session_id: acp::SessionId,
        worktree_path: std::path::PathBuf,
        session_cwd: std::path::PathBuf,
        code_restored: bool,
        restore_summary: Option<String>,
        restore_degree: Option<distill_workspace::session::git::RestoreDegree>,
        /// Resume/parent id this worktree was created from (`load_session_id`).
        /// Used to retarget the one-shot restore-code suppression onto the child.
        resume_session_id: Option<String>,
        strategy_summary: Option<String>,
    },
    /// Worktree session creation failed.
    WorktreeSessionFailed {
        agent_id: AgentId,
        error: String,
        /// The orphaned worktree still on disk, so the handler can re-append the `grok worktree rm` hint; `None` if none was created.
        orphaned_worktree_root: Option<std::path::PathBuf>,
        /// The create RPC hit its bounded timeout, rather than failing early for another reason.
        timed_out: bool,
    },
    /// Session was loaded (resumed) successfully.
    SessionLoaded {
        agent_id: AgentId,
        session_id: acp::SessionId,
        models: Option<acp::SessionModelState>,
        modes: Option<acp::SessionModeState>,
        code_restored: bool,
        restore_summary: Option<String>,
        restore_degree: Option<distill_workspace::session::git::RestoreDegree>,
        /// The session's in-flight running prompt id (from the load response `_meta["x.ai/runningPromptId"]`).
        /// Present only when the session was loaded MID-turn (another client is driving).
        /// The loader adopts it to pass the live `session/update` gate without re-rendering the user block (replay already rendered it).
        running_prompt_id: Option<String>,
    },
    /// Session load (resume) failed.
    SessionLoadFailed {
        agent_id: AgentId,
        session_id: acp::SessionId,
        error: String,
    },
    /// Local `summary.json` display fields for [`Effect::HydrateSessionMetaFromDisk`].
    SessionMetaFromDisk {
        agent_id: AgentId,
        /// The display title paired with whether it came from a manual `/rename` (`summary.title_is_manual`, restores the prompt-border title).
        /// Manual-ness cannot exist without a title.
        title: Option<(String, bool)>,
        /// Persisted per-turn dashboard summary, so a resumed session's row shows it without waiting for the next turn.
        last_turn_summary: Option<String>,
        /// Generation captured when the hydrate effect was enqueued.
        last_turn_summary_gen: u64,
    },
    /// Session list fetched for the welcome screen picker.
    SessionListLoaded {
        /// Echo of [`Effect::FetchSessionList::host`]; results apply only to the requesting host's picker storage.
        host: crate::views::session_picker_surface::SessionPickerHost,
        /// Echo of [`Effect::FetchSessionList::generation`]; results for a superseded picker incarnation are dropped.
        generation: u64,
        sessions: Vec<crate::app::app_view::SessionPickerEntry>,
        /// A degraded conversations lane (`_meta["x.ai/partial"]`), shown as an actionable picker notice instead of a silent empty list.
        partial: Option<crate::app::effects::ConversationsPartial>,
        /// Directory scope `sessions` were drawn from (`x.ai/listScope`).
        scope: distill_shell::session::unified_list::ListScope,
        /// Echo of [`Effect::FetchSessionList::seq`]; stale results are dropped.
        seq: u64,
        /// Echo of [`Effect::FetchSessionList::query`].
        /// `Some` marks the sessions as server-side search results, so the local fuzzy re-filter doesn't hide content-only hits.
        /// Zero hits are then a normal outcome rather than an empty-directory error.
        query: Option<String>,
    },
    /// A background foreign-session scan completed.
    ForeignSessionsScanned {
        entries: Vec<crate::app::app_view::SessionPickerEntry>,
        seq: u64,
    },
    /// Launch cwd canonicalization completed before foreign store access.
    ForeignResumeCwdCanonicalized {
        requested_cwd: std::path::PathBuf,
        canonical_cwd: Option<std::path::PathBuf>,
        launch_token: u64,
    },
    /// Launch-time foreign resume detection completed.
    ForeignResumeHintDetected {
        canonical_cwd: std::path::PathBuf,
        launch_token: u64,
        hint: Option<distill_foreign_sessions::RecentForeignSession>,
    },
    /// Session list fetch failed.
    SessionListFailed {
        /// Echo of [`Effect::FetchSessionList::host`].
        host: crate::views::session_picker_surface::SessionPickerHost,
        /// Echo of [`Effect::FetchSessionList::generation`].
        generation: u64,
        error: String,
        /// Echo of [`Effect::FetchSessionList::seq`]; stale failures are dropped.
        seq: u64,
        /// Echo of [`Effect::FetchSessionList::query`].
        /// `Some` (a failed search) clears the search in-flight indicator.
        /// `None` must leave it alone; in Build mode the flag belongs to the FTS5 deep search.
        query: Option<String>,
    },
    /// Picker search debounce elapsed ([`Effect::DebounceSessionSearch`]).
    SessionSearchDebounceExpired {
        /// Echo of [`Effect::DebounceSessionSearch::host`].
        host: crate::views::session_picker_surface::SessionPickerHost,
        /// Echo of [`Effect::DebounceSessionSearch::generation`].
        generation: u64,
        query: String,
        seq: u64,
    },
    /// Leader session roster loaded via `x.ai/sessions/list`.
    RosterLoaded {
        sessions: Vec<crate::app::roster::RosterEntry>,
    },
    /// Leader session roster fetch failed (silent; no error modal).
    RosterFailed {
        error: String,
    },
    /// Local on-disk session list loaded for the dashboard (non-leader fallback).
    /// Entries are pre-converted to `RosterEntry` (activity `Dormant`) so they reuse the roster-row rendering path.
    /// A fetch failure yields an empty list (silent; the next poll retries).
    DashboardSessionsLoaded {
        sessions: Vec<crate::app::roster::RosterEntry>,
    },
    /// Dashboard v2's process-owned store and initial consistent view.
    WorkspaceSnapshotLoaded {
        store: distill_dashboard_store::WorkspaceStore,
        snapshot: distill_dashboard_store::WorkspaceSnapshot,
    },
    /// Dashboard v2 store open or initial snapshot failed.
    WorkspaceSnapshotFailed {
        error: String,
        /// Only SQLite lock contention is expected to clear without user action.
        retryable: bool,
    },
    WorkspaceWriteCompleted {
        store: distill_dashboard_store::WorkspaceStore,
        completion: WorkspaceWriteCompletion,
    },
    /// The write task lost its moved handle; `db_path` identifies the store to reopen.
    WorkspaceWriteTaskFailed {
        db_path: std::path::PathBuf,
        error: String,
    },
    WorkspaceRefreshed {
        store: distill_dashboard_store::WorkspaceStore,
        snapshot: Result<Option<distill_dashboard_store::WorkspaceSnapshot>, String>,
    },
    /// Reopens the store handle lost by a failed refresh task.
    WorkspaceRefreshTaskFailed {
        db_path: std::path::PathBuf,
        error: String,
    },
    /// Card detail loaded for a session in the picker.
    CardDetailLoaded {
        /// Echo of [`Effect::LoadCardDetail::host`].
        host: crate::views::session_picker_surface::SessionPickerHost,
        /// Echo of [`Effect::LoadCardDetail::generation`].
        generation: u64,
        source: String,
        session_id: String,
        /// Echo of [`Effect::LoadCardDetail::seq`].
        seq: u64,
        detail: crate::app::app_view::CardDetail,
    },
    /// Remote session restored successfully; now load it.
    /// Always a Build disk row (see [`Effect::RestoreAndLoadSession`]).
    SessionRestored {
        agent_id: AgentId,
        /// The local session ID (may differ from remote ID).
        local_session_id: String,
    },
    /// Remote session restore failed.
    SessionRestoreFailed {
        agent_id: AgentId,
        error: String,
    },
    /// Incremental progress during remote session restore.
    SessionRestoreProgress {
        agent_id: AgentId,
        message: String,
    },
    /// Prompt response received (turn ended).
    PromptResponse {
        agent_id: AgentId,
        result: Result<acp::PromptResponse, String>,
        /// HTTP status code from the upstream API error, if available.
        /// Used by dispatch to show targeted UI (e.g. credit-limit upsell on 403).
        http_status: Option<u16>,
        /// This is therefore the ONLY way to attribute an *error* response to its prompt.
        /// The dispatch gate uses it to discard errors from queued/stale prompts instead of painting them onto the running turn.
        /// `None` for synthetic/test constructions that don't need gating.
        prompt_id: Option<String>,
    },
    /// A send-now `session/prompt` RPC failed at the transport/RPC layer; the prompt never reached the shell's queue.
    /// Carries the payload so dispatch can requeue it locally.
    /// The producer already consumed the composer/queue row, so dropping it would silently lose the message.
    SendPromptNowFailed {
        agent_id: AgentId,
        session_id: acp::SessionId,
        prompt_id: String,
        error: String,
        blocks: Vec<acp::ContentBlock>,
    },
    /// Cancel notification was sent (fire-and-forget).
    /// The real turn end comes via PromptResponse.
    CancelComplete,
    /// `session/set_mode` failed. Clear optimistic `plan_mode_pending` and
    /// `pending_post_turn_commit` so abandon/leave can retry; keep and review stay mounted.
    SetSessionModeFailed {
        session_id: acp::SessionId,
    },
    /// The marker can stop advertising itself as unsent.
    ConsentRecorded {
        notice_id: String,
        version: i32,
    },
    /// The answer stands for this run, but nothing on disk holds it, so the notice returns at the next launch.
    ConsentPersistFailed {
        error: String,
    },
    /// Response to `x.ai/subagent/cancel`; see [`SubagentKillOutcome`].
    KillSubagentComplete {
        session_id: acp::SessionId,
        subagent_id: String,
        attempt_id: Option<String>,
        outcome: SubagentKillOutcome,
    },
    PreferredModelPersisted {
        result: Result<(), String>,
    },
    /// Manual `/compact` command completed.
    CompactComplete {
        agent_id: AgentId,
        result: Result<(), crate::app::effects::CompactError>,
    },
    /// Background task kill result.
    /// `outcome` is `None` when the agent returned an error envelope or an unparseable payload (treated as "clear pending state, keep the row").
    BgTaskKilled {
        session_id: String,
        task_id: String,
        outcome: Option<distill_tools::types::KillOutcome>,
    },
    /// Background task kill failed.
    BgTaskKillFailed {
        session_id: String,
        task_id: String,
        error: String,
    },
    /// Model switch completed (effort, if any, was applied in the same request).
    SwitchModelComplete {
        agent_id: AgentId,
        model_id: acp::ModelId,
        effort: Option<ReasoningEffort>,
        result: Result<Option<u64>, SwitchModelError>,
        /// Forwarded from `Effect::SwitchModel.prev_model_id` for rollback on `IncompatibleAgent`.
        prev_model_id: Option<acp::ModelId>,
    },
    /// Auto effort request finished; a rejection rolls the optimistic flag back.
    EffortAutoSet {
        agent_id: AgentId,
        model_id: acp::ModelId,
        result: Result<(), String>,
    },
    /// Changelog fetched from CDN (both formats).
    ChangelogFetched {
        markdown: Option<String>,
        entries: Vec<distill_shell::util::changelog::ChangelogEntry>,
    },
    /// Announcements hidden state persisted.
    AnnouncementsHiddenPersisted {
        result: Result<(), String>,
    },
    /// Cross-session prompt history loaded from ACP.
    PromptHistoryLoaded {
        agent_id: AgentId,
        prompts: Vec<String>,
    },
    /// Running agent name cached from `session/info` (for agents modal, etc.).
    SessionAgentNameResolved {
        agent_id: AgentId,
        agent_name: Option<String>,
    },
    /// Authentication completed successfully.
    AuthComplete {
        request_seq: u64,
        meta: Option<serde_json::Value>,
    },
    /// Authentication failed.
    AuthFailed {
        request_seq: u64,
        error: String,
    },
    /// Auth URL is ready (from the provider).
    AuthUrlReady {
        request_seq: u64,
        auth_url: Option<String>,
        /// Deprecated: superseded by `mode` (authoritative).
        /// Kept only as a back-compat fallback for older agents that don't send `mode`.
        external: bool,
        /// Presentation mode from `x.ai/auth/get_url`; `None` on older agents.
        mode: Option<String>,
    },
    /// Auth code was submitted (fire-and-forget).
    AuthCodeSubmitted {
        request_seq: u64,
    },
    /// MCP server list fetched from shell.
    McpsListLoaded {
        agent_id: AgentId,
        result: Result<Vec<crate::views::mcps_modal::McpServerInfo>, String>,
    },
    /// MCP auth trigger completed.
    McpAuthTriggerDone {
        agent_id: AgentId,
        server_name: String,
        result: Result<McpAuthTriggerOutcome, String>,
    },
    McpSetupSubmitDone {
        agent_id: AgentId,
        server_name: String,
        result: Result<(), String>,
    },
    /// Hooks list fetched from shell.
    HooksListLoaded {
        agent_id: AgentId,
        result: Result<distill_hooks_plugins_types::HooksListResponse, String>,
    },
    /// Plugins list fetched from shell.
    PluginsListLoaded {
        agent_id: AgentId,
        result: Result<distill_hooks_plugins_types::PluginsListResponse, String>,
    },
    /// Memory listing fetched for the `/memory` modal.
    MemoryListLoaded {
        agent_id: AgentId,
        result: Result<distill_shell::extensions::memory::MemoryListing, String>,
    },
    /// Shell answered a memory on/off request.
    MemoryToggleResult {
        agent_id: AgentId,
        result: Result<distill_shell::extensions::memory::MemoryToggleResponse, String>,
    },
    /// Shell answered a `/memory` modal delete request.
    MemoryForgetResult {
        agent_id: AgentId,
        path: String,
        result: Result<distill_shell::extensions::memory::MemoryForgetResponse, String>,
    },
    /// `/flush` finished.
    MemoryFlushComplete {
        agent_id: AgentId,
        result: Result<distill_shell::extensions::memory::MemoryFlushResponse, String>,
    },
    /// `/dream` finished.
    MemoryDreamComplete {
        agent_id: AgentId,
        result: Result<distill_shell::extensions::memory::MemoryDreamResponse, String>,
    },
    /// Hooks action completed.
    HooksActionResult {
        agent_id: AgentId,
        result: Result<distill_hooks_plugins_types::ActionOutcome, String>,
    },
    /// Plugins action completed.
    PluginsActionResult {
        agent_id: AgentId,
        result: Result<distill_hooks_plugins_types::ActionOutcome, String>,
    },
    /// Marketplace list loaded.
    MarketplaceListLoaded {
        agent_id: AgentId,
        result: Result<distill_hooks_plugins_types::MarketplaceListResponse, String>,
    },
    /// Official-marketplace CTA catalog loaded into agent-level state.
    PluginCtaCatalogLoaded {
        agent_id: AgentId,
        result: Result<distill_hooks_plugins_types::MarketplaceListResponse, String>,
    },
    /// Skills list loaded.
    SkillsListLoaded {
        agent_id: AgentId,
        result: Result<Vec<distill_tools::implementations::skills::types::SkillInfo>, String>,
    },
    WorkflowsListLoaded {
        agent_id: AgentId,
        session_id: acp::SessionId,
        result: Result<Vec<crate::views::extensions_modal::WorkflowInfo>, String>,
    },
    /// Skill toggle completed (enable/disable).
    SkillsToggleDone {
        agent_id: AgentId,
        result: Result<Vec<distill_tools::implementations::skills::types::SkillInfo>, String>,
    },
    /// Background marketplace auto-update completed.
    MarketplaceUpdatesAvailable {
        agent_id: AgentId,
        updates: Vec<(String, String, String)>,
    },
    /// Marketplace action completed.
    MarketplaceActionResult {
        agent_id: AgentId,
        result: Result<distill_hooks_plugins_types::ActionOutcome, String>,
    },
    /// Inline-CTA plugin install completed.
    CtaPluginInstallDone {
        agent_id: AgentId,
        plugin_name: String,
        result: Result<distill_hooks_plugins_types::ActionOutcome, String>,
    },
    /// Post-CTA-install plugins reload completed (modal-independent).
    CtaPluginReloadDone {
        agent_id: AgentId,
        plugin_name: String,
        result: Result<distill_hooks_plugins_types::ActionOutcome, String>,
    },
    /// Post-CTA-install MCP server list loaded (modal-independent).
    PluginCtaMcpsLoaded {
        agent_id: AgentId,
        plugin_name: String,
        result: Result<Vec<crate::views::mcps_modal::McpServerInfo>, String>,
    },
    /// The CTA "installed" confirmation auto-dismiss timer fired.
    CtaInstalledDismissTimeout {
        agent_id: AgentId,
        plugin_name: String,
    },
    /// Live MCP toggle completed.
    McpToggleDone {
        agent_id: AgentId,
        result: Result<(), String>,
    },
    /// Share session completed successfully.
    ShareSessionComplete {
        agent_id: AgentId,
        share_url: String,
    },
    /// Share session failed.
    ShareSessionFailed {
        agent_id: AgentId,
        error: String,
    },
    /// Session info fetched successfully.
    SessionInfoComplete {
        agent_id: AgentId,
        session_id: acp::SessionId,
        info: Box<distill_shell::session::SessionInfoResponse>,
        /// Plain-text block for minimal-mode scrollback.
        text: String,
        /// Structured rows for the modal (built upstream from typed data).
        fields: Vec<crate::views::usage_modal::SessionInfoField>,
        nonce: u64,
    },
    /// Session info fetch failed.
    SessionInfoFailed {
        agent_id: AgentId,
        session_id: acp::SessionId,
        error: String,
        nonce: u64,
    },
    /// Coding data sharing preference updated.
    CodingDataSharingUpdated {
        agent_id: AgentId,
        opted_in: bool,
        seq: u64,
    },
    /// Coding data sharing update failed.
    CodingDataSharingFailed {
        agent_id: AgentId,
        error: String,
        seq: u64,
    },
    /// Session rename completed successfully.
    RenameSessionComplete {
        agent_id: AgentId,
        title: String,
    },
    /// Session rename failed.
    RenameSessionFailed {
        agent_id: AgentId,
        error: String,
    },
    /// `/rename --auto` completed successfully.
    ResetSessionTitleComplete {
        agent_id: AgentId,
    },
    /// `/rename --auto` failed.
    ResetSessionTitleFailed {
        agent_id: AgentId,
        error: String,
        previous_display_name: Option<String>,
        previous_generated_title: Option<String>,
    },
    /// Session delete completed successfully.
    DeleteSessionComplete {
        source: String,
        session_id: String,
        after: AfterSessionDelete,
    },
    /// Session delete failed.
    DeleteSessionFailed {
        source: String,
        session_id: String,
        error: String,
    },
    /// Context info fetched successfully. Drop if `session_id` no longer matches.
    ContextInfoComplete {
        agent_id: AgentId,
        session_id: acp::SessionId,
        info: Box<distill_shell::session::SessionInfoResponse>,
        nonce: u64,
    },
    /// Context info fetch failed. Drop if `session_id` no longer matches.
    ContextInfoFailed {
        agent_id: AgentId,
        session_id: acp::SessionId,
        error: String,
        nonce: u64,
    },
    /// `/usage` session ledger fetched. Drop if `session_id` no longer matches.
    SessionUsageComplete {
        agent_id: AgentId,
        session_id: acp::SessionId,
        usage: Box<distill_shell::extensions::notification::PromptUsage>,
        nonce: u64,
    },
    /// `/usage` session ledger fetch failed. Drop if `session_id` no longer matches.
    SessionUsageFailed {
        agent_id: AgentId,
        session_id: acp::SessionId,
        error: String,
        nonce: u64,
    },
    /// Feedback submitted successfully (fire-and-forget).
    /// A modal-origin completion only takes the consent parked for its exact submission; anything else is stale and a no-op.
    FeedbackComplete {
        agent_id: AgentId,
        origin: FeedbackSendOrigin,
        outcome: distill_shell::session::FeedbackOutcome,
        /// Present only when the shell consumed explicit modal consent and minted a one-shot capability.
        trace_upload_token: Option<String>,
    },
    /// Feedback submission failed. A draft send keeps its draft on disk; any other report is
    /// re-saved as a text-only draft from `feedback_text` so the user can retry from `/feedback`.
    /// A modal-origin failure additionally drops its parked consent so a failed report never uploads a trace.
    FeedbackFailed {
        agent_id: AgentId,
        origin: FeedbackSendOrigin,
        feedback_text: String,
        /// Attachments the POST carried; they are not re-saved with the draft.
        image_count: usize,
        error: String,
    },
    FeedbackDraftListComplete {
        agent_id: AgentId,
        modal_id: crate::views::feedback_modal::FeedbackModalId,
        generation: u64,
        result: Result<Vec<distill_feedback::FeedbackDraft>, String>,
    },
    FeedbackDraftLoadComplete {
        agent_id: AgentId,
        load: crate::views::feedback_modal::FeedbackDraftLoad,
        result: Result<distill_feedback::FeedbackDraft, String>,
    },
    FeedbackDraftDeleteComplete {
        agent_id: AgentId,
        delete: crate::views::feedback_modal::FeedbackDraftDelete,
        result: Result<(), String>,
    },
    FeedbackDraftUpdateComplete {
        agent_id: AgentId,
        update: crate::views::feedback_modal::FeedbackDraftUpdate,
        result: Result<(), String>,
    },
    /// One-shot feedback trace archive finished (or was skipped).
    /// `submission_id` echoes the effect's correlation: a modal one-shot completion only ever
    /// touches its registered pending submission, never a later modal.
    FeedbackTraceUploaded {
        agent_id: AgentId,
        submission_id: Option<crate::views::feedback_modal::FeedbackSubmissionId>,
        error: Option<String>,
    },
    /// Memory note save completed.
    MemoryNoteSaved {
        agent_id: AgentId,
        result: Result<(), String>,
    },
    /// LLM-rewritten memory note ready for inline review.
    /// `Ok(text)` carries the rewritten markdown; `Err(error)` means the rewrite failed.
    MemoryNoteRewritten {
        agent_id: AgentId,
        result: Result<String, String>,
        /// Nonce from the originating RewriteMemoryNote effect; must match the modal's `rewrite_nonce` before populating enhanced_content.
        nonce: u64,
    },
    /// Bundle status fetched successfully.
    BundleStatusReady {
        has_cache: bool,
        version: Option<String>,
        personas: Vec<String>,
        roles: Vec<String>,
        agents: Vec<String>,
        skills: Vec<String>,
        persona_details: Vec<super::bundle::PersonaDetail>,
        role_details: Vec<super::bundle::RoleDetail>,
    },
    /// Bundle status fetch failed.
    BundleStatusFailed {
        error: String,
    },
    /// Catalog entry content fetched successfully.
    CatalogEntryReady {
        kind: String,
        name: String,
        content: String,
    },
    /// Catalog entry fetch failed.
    CatalogEntryFailed {
        error: String,
    },
    /// Side question (/btw) response received.
    BtwResponse {
        agent_id: AgentId,
        result: Result<String, String>,
        /// Correlates minimal responses; fullscreen leaves this unset.
        minimal_request_id: Option<uuid::Uuid>,
        /// Set when attached images were left out of the side question.
        image_notice: Option<String>,
    },
    /// `x.ai/recap` request acknowledged (fire-and-forget).
    /// The recap itself arrives separately as a `SessionRecap` notification; this only carries a transport error, if any, for logging.
    RecapRequested {
        /// Session the recap was requested for; lets the handler find the agent whose manual loading spinner must be cleared on failure.
        session_id: acp::SessionId,
        /// Whether this was an automatic recap.
        /// Only a manual `/recap` shows a loading spinner, so only a manual failure needs to clear one.
        auto: bool,
        error: Option<String>,
    },
    /// Interjection queued acknowledgement.
    InterjectQueued {
        agent_id: AgentId,
    },
    /// Interjection send failed.
    /// Carries the payload so the dispatcher can requeue it (mirrors the batch path's `failed_local` requeue).
    /// The queue row was already removed optimistically, so dropping the text here would silently lose the user's message.
    InterjectFailed {
        agent_id: AgentId,
        error: String,
        remaining: Vec<(
            String,
            String,
            Option<Vec<agent_client_protocol::ContentBlock>>,
        )>,
    },
    /// Available commands refreshed from the shell.
    AvailableCommandsRefreshed {
        agent_id: AgentId,
        commands: Vec<acp::AvailableCommand>,
    },
    /// Shell acknowledged logout (auth cleared).
    LogoutComplete,
    LogoutFailed(String),
    /// Best-effort `x.ai/auth/cancel` finished (no UI update; state already left Authenticating).
    AuthCancelComplete,
    /// Shell responded to `x.ai/auth/check_subscription`.
    /// `verify` echoes the generation from `Effect::CheckSubscription` for deferred-gate verifications.
    CheckSubscriptionComplete {
        verify: Option<u64>,
        meta: Option<serde_json::Value>,
    },
    /// Result of the credit-limit subscription re-check.
    /// If the tier changed the stashed prompt is retried; otherwise the upsell is shown.
    CreditLimitRecheckComplete {
        agent_id: AgentId,
        meta: Option<serde_json::Value>,
    },
    /// 5s paywall check timer fired: time to send another check.
    PaywallCheckTick,
    /// The deferred-gate verification window expired.
    GateVerifyTimeout {
        generation: u64,
    },
    /// The 2-second auth copy feedback timer expired.
    AuthCopyFeedbackTimeout {
        generation: u64,
    },
    DeepSearchResults {
        /// Echo of [`Effect::DeepSearchSessions::host`].
        host: crate::views::session_picker_surface::SessionPickerHost,
        /// Echo of [`Effect::DeepSearchSessions::generation`].
        generation: u64,
        results: Vec<distill_shell::extensions::session_search::SearchSessionHit>,
        seq: u64,
    },
    /// `x.ai/session/fork` completed (no-worktree path).
    /// The pager adopts the new session id and emits [`Effect::LoadSession`] to start the replay.
    /// Mirrors [`TaskResult::WorktreeForked`] in shape.
    ForkSessionReady {
        agent_id: AgentId,
        new_session_id: acp::SessionId,
        cwd: std::path::PathBuf,
        /// Parent session id the fork was taken from (to retarget the one-shot restore-code suppression).
        parent_session_id: acp::SessionId,
    },
    /// `x.ai/session/fork` failed.
    /// The placeholder agent stays in `app.agents` with no `session_id` so the user can switch away.
    ForkSessionFailed {
        agent_id: AgentId,
        error: String,
    },
    RewindPointsLoaded {
        agent_id: AgentId,
        points: Vec<crate::views::rewind::RewindPointInfo>,
    },
    RewindPointsFailed {
        agent_id: AgentId,
        error: String,
    },
    RewindExecuteComplete {
        agent_id: AgentId,
        response: crate::views::rewind::RewindResponse,
    },
    RewindExecuteFailed {
        agent_id: AgentId,
        error: String,
    },
    /// Billing data fetched from the agent.
    BillingFetched {
        agent_id: AgentId,
        balance: Option<crate::views::credit_bar::CreditBalance>,
        usage_available: bool,
        /// When true, update `credit_balance` silently (no scrollback message).
        silent: bool,
        /// Subscription tier piggybacked from remote settings.
        subscription_tier: Option<String>,
        /// Auto top-up rule fetch result; `Unchanged` keeps any cached rule.
        autotopup: crate::views::credit_bar::AutoTopupFetch,
        /// Usage-modal fetch generation (`0` means a background refresh).
        nonce: u64,
    },
    /// App-level billing data (welcome screen, dashboard usage modal).
    AppBillingFetched {
        balance: Option<crate::views::credit_bar::CreditBalance>,
        usage_available: bool,
        autotopup: crate::views::credit_bar::AutoTopupFetch,
        /// Usage-modal fetch generation (`0` means a background refresh).
        nonce: u64,
    },
    /// App-level billing fetch failed (transport or parse); the cached balance is kept.
    AppBillingError {
        error: String,
        /// Usage-modal fetch generation (`0` means a background refresh).
        nonce: u64,
    },
    /// ChatGPT account usage fetched from the isolated Codex provider.
    ChatGptUsageResult {
        usage: Result<Box<distill_shell::codex_auth::CodexUsageSnapshot>, String>,
        agent_id: Option<AgentId>,
        nonce: u64,
    },
    /// OpenRouter per-key usage fetched from its isolated provider client.
    OpenRouterUsageResult {
        usage: Result<distill_shell::openrouter_auth::OpenRouterUsage, String>,
        agent_id: Option<AgentId>,
        nonce: u64,
    },
    GateRefreshed {
        settings: Option<distill_shell::util::config::RemoteSettings>,
    },
    /// Billing fetch failed with an error message.
    BillingError {
        agent_id: AgentId,
        error: String,
        /// When true, swallow the error silently (background refresh).
        silent: bool,
        /// Usage-modal fetch generation (`0` means a background refresh).
        nonce: u64,
    },
    /// Debounce timer for shell suggestions expired.
    /// Routed by the arming `agent_id`, like the sibling `PluginCtaDebounceExpired`.
    SuggestionDebounceExpired {
        agent_id: AgentId,
        generation: u64,
    },
    /// Debounce timer for plugin-CTA keyword matching expired.
    PluginCtaDebounceExpired {
        agent_id: AgentId,
        generation: u64,
    },
    /// Shell suggestions loaded from ACP `x.ai/suggest`.
    /// `request_text` / `request_cursor` echo what the request was built from, paired atomically with the items.
    /// They are the anchor the items' `replaceRange` offsets index into and the position Tab targets.
    ShellSuggestionsLoaded {
        agent_id: AgentId,
        response: crate::views::suggestion_controller::SuggestResponseParsed,
        request_text: String,
        request_cursor: usize,
    },
    /// Predicted next prompt loaded from ACP `x.ai/suggestPrompt`.
    /// `suggestion` is `None` when the shell had nothing to suggest.
    PromptSuggestionLoaded {
        agent_id: AgentId,
        suggestion: Option<String>,
        generation: u64,
    },
    /// Setting persisted successfully. No reconciliation needed today.
    SettingPersisted {
        key: crate::settings::SettingKey,
        value: crate::settings::SettingValue,
    },
    /// Setting persist failed.
    /// Rolls back in-memory cache to `rollback_value` and shows a failure toast.
    SettingPersistFailed {
        key: crate::settings::SettingKey,
        rollback_value: crate::settings::SettingValue,
        error: String,
    },
    /// Best-effort persist failed (cycle_mode path).
    /// Logs and toasts but does NOT roll back in-memory state.
    SettingPersistFailedBestEffort {
        key: crate::settings::SettingKey,
        error: String,
    },
    /// Off-thread clipboard attachment probe finished (see [`Effect::ProbeClipboardAttachment`]); dispatch attaches the chip.
    ClipboardAttachmentProbed {
        ctx: ClipboardPasteContext,
        /// Decoded/persisted image outcome from the off-thread probe.
        image: ProbedAttachment,
        /// File URL(s) the completion resolves via the existing path handling.
        file_urls: Option<String>,
    },
    FeedbackImageRehydrated {
        agent_id: AgentId,
        modal_id: crate::views::feedback_modal::FeedbackModalId,
        image_identity: u64,
        result: Result<Vec<u8>, String>,
    },
    /// Shared prompt-image preview state was resolved off-thread.
    PromptImagePreviewPrepared,
    DoctorFixPlanned {
        target: DoctorFixTarget,
        result: Result<DoctorPlanningOutcome, String>,
    },
    DoctorFixApplied {
        target: DoctorFixTarget,
        result: Result<crate::diagnostics::FixOutcome, String>,
    },
}
#[cfg(test)]
mod tests {
    use super::*;
    /// `as_canonical` must return the wire strings that the settings picker and dispatcher pattern-match against.
    #[test]
    fn plan_mode_kind_as_canonical() {
        assert_eq!(PlanModeKind::On.as_canonical(), "on");
        assert_eq!(PlanModeKind::Off.as_canonical(), "off");
    }
    /// `as_display` must return user-visible picker labels.
    #[test]
    fn plan_mode_kind_as_display() {
        assert_eq!(PlanModeKind::On.as_display(), "On");
        assert_eq!(PlanModeKind::Off.as_display(), "Off");
    }
    /// `to_bool` must map `On` to `true` and `Off` to `false`.
    #[test]
    fn plan_mode_kind_to_bool() {
        assert!(PlanModeKind::On.to_bool());
        assert!(!PlanModeKind::Off.to_bool());
    }
    /// `from_bool` must be the inverse of `to_bool`.
    #[test]
    fn plan_mode_kind_from_bool_round_trip() {
        assert_eq!(PlanModeKind::from_bool(true), PlanModeKind::On);
        assert_eq!(PlanModeKind::from_bool(false), PlanModeKind::Off);
        for b in [true, false] {
            assert_eq!(
                PlanModeKind::from_bool(b).to_bool(),
                b,
                "from_bool ∘ to_bool round-trip must be identity",
            );
        }
        for k in [PlanModeKind::On, PlanModeKind::Off] {
            assert_eq!(
                PlanModeKind::from_bool(k.to_bool()),
                k,
                "to_bool ∘ from_bool round-trip must be identity",
            );
        }
    }
    /// Drift-guard: `PLAN_MODE_CHOICES` canonicals must match `PlanModeKind::as_canonical`.
    #[test]
    fn plan_mode_kind_canonical_strings_match_choices_catalog() {
        let catalog_canonicals: std::collections::HashSet<&str> =
            crate::settings::defs::default_settings()
                .iter()
                .find(|m| m.key == "plan_mode")
                .map(|m| match &m.kind {
                    crate::settings::SettingKind::Enum { choices, .. } => {
                        choices.iter().map(|c| c.canonical).collect()
                    }
                    _ => panic!("plan_mode must be Enum"),
                })
                .expect("plan_mode must be registered");
        assert!(
            catalog_canonicals.contains(PlanModeKind::On.as_canonical()),
            "catalog must contain `{}` (from PlanModeKind::On)",
            PlanModeKind::On.as_canonical(),
        );
        assert!(
            catalog_canonicals.contains(PlanModeKind::Off.as_canonical()),
            "catalog must contain `{}` (from PlanModeKind::Off)",
            PlanModeKind::Off.as_canonical(),
        );
        assert_eq!(
            catalog_canonicals.len(),
            2,
            "catalog must be exactly {{on, off}} — adding a third \
             choice requires adding a PlanModeKind variant AND \
             updating the action_for_enum_commit + action_for_reset \
             dispatcher arms",
        );
    }
}
