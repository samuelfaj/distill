//! Dispatch is synchronous: `run()` has no `async_trait`, and commands that need async work return `CommandResult::Action(action)` for the dispatch layer.
//! Trait methods return `&str` (not `&'static str`) so ACP-sourced commands with runtime-determined data work.
//! There is no `validate_args()`; validation happens inside `run()`.

use crate::acp::model_state::ModelState;
use crate::app::actions::Action;
use crate::app::bundle::BundleState;
use crate::slash::mode_support::ModeSupport;
use agent_client_protocol as acp;

/// Provisional scheduled task info for immediate display in the tasks pane.
///
/// Created by `/loop` when the user submits the command so the task appears instantly, rather than waiting for the LLM round-trip through `scheduler_create`.
#[derive(Debug, Clone)]
pub struct ScheduledTaskPreview {
    pub prompt: String,
    pub human_schedule: String,
    pub next_fire_at: Option<String>,
    /// Tag shown in the tasks pane (e.g. "loop", "check"). Defaults to "loop".
    pub tag: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DoctorRequest {
    Report,
    ListFixes,
    Fix(crate::diagnostics::DiagnosticId),
}

/// Result of running a slash command.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum CommandResult {
    /// Command handled successfully, no visible output needed.
    /// Kept for parity with the original TUI's result enum; no command returns it directly today.
    Handled,
    /// Build or act on TUI doctor state from live app/session inputs.
    Doctor(DoctorRequest),
    /// Command failed with an error message.
    Error(String),
    /// Command produced a user-visible message.
    Message(String),
    /// Command produced a pager Action to dispatch (e.g., SwitchModel, Quit).
    Action(Action),
    /// Command should be sent through the queued command pipeline (e.g., /compact).
    /// The String is the raw command text.
    QueueCommand(String),
    /// Skill invocation: pager read the SKILL.md, applied substitutions, and constructed structured prompt blocks for the wire.
    /// `display_text` is what the user sees in scrollback.
    /// `prompt_blocks` is the actual content sent to the model.
    InjectSkill {
        display_text: String,
        prompt_blocks: Vec<agent_client_protocol::ContentBlock>,
        /// Whether to display as a skill invocation (teal accent) in scrollback.
        /// `true` for real skills (e.g. /commit), `false` for built-in commands like /loop that inject structured prompts but aren't skills.
        display_as_skill: bool,
        /// If set, immediately show a provisional scheduled task in the tasks pane.
        /// The real `ScheduledTaskCreated` notification from the shell replaces it.
        scheduled_task_preview: Option<ScheduledTaskPreview>,
    },
    /// Command text should be sent as a regular prompt. This variant deliberately covers two different cases. Unknown
    /// commands (pager doesn't know them, shell might).
    PassThrough(String),
}

/// A suggestion item for command argument completion.
#[derive(Debug, Clone)]
pub struct ArgItem {
    /// Display text shown in the dropdown.
    pub display: String,
    /// Text used for fuzzy matching.
    pub match_text: String,
    /// Text inserted into the prompt on acceptance.
    pub insert_text: String,
    /// Description shown alongside the item.
    pub description: String,
}

/// A saved or built-in workflow the `/workflow` picker can launch, sourced from ACP commands that carry `_meta.workflowSource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowChoice {
    pub name: String,
    pub description: String,
}

/// A session workflow run the `/workflow` manage verbs can target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunChoice {
    pub name: String,
    pub status: String,
    pub builtin: bool,
}

impl WorkflowRunChoice {
    pub fn can_pause(&self) -> bool {
        self.status == "active"
    }

    pub fn can_resume(&self) -> bool {
        // The picker lists only runs the user stopped (`/workflow stop`) or paused (`/workflow pause`)
        // System pauses (blocked, back-off, budget) stay off this list; those runs are still resumable if typed by name
        matches!(self.status.as_str(), "user_paused" | "cancelled")
    }

    pub fn can_stop(&self) -> bool {
        !matches!(
            self.status.as_str(),
            "interrupted" | "complete" | "failed" | "cancelled"
        )
    }

    pub fn can_save(&self, definitions: &[WorkflowChoice]) -> bool {
        // Shell save requires the display name to equal the script's `meta.name`
        // First runs keep the catalog name; uniquified copies (`review-pr-2`) do not
        // A definition literally named `sprint-2` is still savable because that name is in the catalog
        !self.builtin
            && definitions
                .iter()
                .any(|workflow| workflow.name == self.name)
    }
}

impl WorkflowChoice {
    /// `None` when the command is not a workflow definition.
    pub fn from_acp(cmd: &acp::AvailableCommand) -> Option<Self> {
        cmd.meta.as_ref()?.get("workflowSource")?;
        let description = cmd
            .description
            .strip_prefix("Workflow: ")
            .unwrap_or(&cmd.description)
            .to_string();
        Some(Self {
            name: cmd.name.clone(),
            description,
        })
    }
}

/// Read-only context for generating suggestions.
/// Passed to `SlashCommand::suggest_args()` and `SlashCommand::visible()`.
/// Kept minimal; extend as needed.
pub struct AppCtx<'a> {
    pub models: &'a ModelState,
    /// Working directory of the active session (for filesystem completions).
    pub cwd: &'a std::path::Path,
    /// Whether any session announcement (critical or promo) exists; gates `/announcements` visibility.
    pub has_session_announcements: bool,
    /// Whether the consumer billing surface is visible (`AppView::usage_visible`); gates `/usage` subcommands.
    pub billing_surface_visible: bool,
    /// Whether `/usage` is offered and executable.
    /// False for external-auth deployments with no grok.com billing session.
    pub usage_command_visible: bool,
    pub workflows_available: bool,
    /// Saved or built-in workflow definitions advertised by the shell (`_meta.workflowSource`).
    /// Backs `/workflow` argument suggestions.
    pub saved_workflows: &'a [WorkflowChoice],
    /// Live session runs.
    /// Backs `/workflow pause|resume|stop|save` name suggestions so a manage verb never auto-picks a run.
    pub workflow_runs: &'a [WorkflowRunChoice],
    /// Effective render mode of this process (gates `/minimal` and `/fullscreen` visibility).
    /// Same source of truth as [`CommandExecCtx::screen_mode`], carried by the owning [`SlashController`](crate::slash::SlashController).
    pub(crate) screen_mode: crate::app::ScreenMode,
    /// Current session title for `/rename` ghost-prefill (`display_name`, else `generated_session_title`).
    /// `None` when there is no title yet.
    pub current_title: Option<&'a str>,
}

/// Mutable execution context for `SlashCommand::run()`.
/// Wraps only what pager can cleanly provide.
/// Commands that need async ACP calls return `CommandResult::Action(...)` and let dispatch handle the effect.
pub struct CommandExecCtx<'a> {
    pub models: &'a ModelState,
    pub session_id: Option<&'a acp::SessionId>,
    pub bundle_state: &'a BundleState,
    pub(crate) screen_mode: crate::app::ScreenMode,
    /// Whether the consumer billing surface is visible (`AppView::usage_visible`); gates `/usage` subcommands.
    pub billing_surface_visible: bool,
    /// Whether `/usage` is offered and executable.
    /// False for external-auth deployments with no grok.com billing session.
    pub usage_command_visible: bool,
    /// Snapshot of the active agent's PAGER-owned settings, built by the dispatcher when it builds the command.
    /// Slash commands like `/multiline` read this to compute `!current` and dispatch a typed `Action::SetX(new)`.
    /// The dispatcher remains the single source of truth for the actual state mutation.
    pub(crate) pager_state: crate::settings::PagerLocalSnapshot,
}

/// Origin of a slash command.
/// The dropdown renderer turns this into badge text via [`CommandProvenance::badge`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandProvenance {
    Builtin,
    /// Non-skill ACP command (e.g. `/flush`).
    Shell,
    /// Skill; `source` is the plugin install name or scope.
    Skill {
        source: String,
    },
}

impl CommandProvenance {
    /// Right-aligned slash-menu badge.
    /// Shell commands render as `built-in` too: the badge only has to separate a skill from whoever kept the bare name.
    pub fn badge(&self) -> std::borrow::Cow<'static, str> {
        match self {
            Self::Builtin | Self::Shell => std::borrow::Cow::Borrowed("built-in"),
            Self::Skill { source } => std::borrow::Cow::Owned(format!("skill · {source}")),
        }
    }
}

/// Implementors define command metadata (name, description, args) and synchronous execution logic.
/// The trait uses `&str` returns (not `&'static str`) so ACP-sourced commands with runtime-determined data work.
pub trait SlashCommand: Send + Sync {
    /// Canonical command name (without leading `/`). E.g., `"exit"`.
    fn name(&self) -> &str;

    /// Alternative names for this command. E.g., `&["quit"]` for `/exit`.
    fn aliases(&self) -> &[&str] {
        &[]
    }

    /// Short human-readable description shown in the dropdown.
    fn description(&self) -> &str;

    /// Origin for the slash-menu provenance badge. Defaults to builtin.
    fn provenance(&self) -> CommandProvenance {
        CommandProvenance::Builtin
    }

    /// Refuse before the submit path mutates the composer or stops voice input.
    fn submission_refusal(&self, _args: &str, _voice_owns_prompt: bool) -> Option<&'static str> {
        None
    }

    /// Usage string shown in help. E.g., `"/model <name>"`.
    fn usage(&self) -> &str;

    /// Whether the command accepts arguments at all.
    fn takes_args(&self) -> bool {
        false
    }

    /// Whether the command accepts arguments right now. Only dropdown and completion paths consult this: the insert
    /// text's trailing space, the snapshot taken when the args phase starts, and argument suggestions.
    #[allow(unused_variables)]
    fn takes_args_now(&self, ctx: &AppCtx) -> bool {
        self.takes_args()
    }

    /// Whether arguments are required for execution. Only meaningful when `takes_args()` is true.
    fn args_required(&self) -> bool {
        false
    }

    /// Generate argument suggestions.
    /// `args_query` is the raw typed args text; most impls ignore it and return a static list.
    #[allow(unused_variables)]
    fn suggest_args(&self, ctx: &AppCtx, args_query: &str) -> Option<Vec<ArgItem>> {
        None
    }

    /// Whether this command is currently visible / executable.
    /// Default is `true` (every command is visible).
    /// Override to gate a command on session state.
    #[allow(unused_variables)]
    fn visible(&self, ctx: &AppCtx) -> bool {
        true
    }

    /// Whether this command operates on a single agent session (its conversation, context, model, turns, plan, etc.)
    /// rather than the pager as a whole. Surfaces that always have a session (the agent view) ignore this flag and
    /// continue to show every command.
    fn session_scoped(&self) -> bool {
        false
    }

    /// Whether a `session_scoped()` command should still be offered on session-less surfaces (the agent dashboard's
    /// dispatch input). Has no effect for non-session-scoped commands (they're always offered).
    fn offered_when_session_less(&self) -> bool {
        false
    }

    /// Whether this command should ONLY be offered on the session-less dashboard surface, the inverse of
    /// [`Self::session_scoped`]. `/cd` changes where the dashboard dispatches new agents, so it is meaningless in an
    /// agent session and hidden there.
    fn dashboard_only(&self) -> bool {
        false
    }

    /// A mid-text `/name` token runs this command with the whole message as its args: `prose /name q` is `/name prose q`.
    /// Opt in only when the whole message is the argument; `submission_refusal` only sees a leading `/` and is bypassed.
    fn can_hoist_from_mid_text(&self) -> bool {
        false
    }

    /// A few commands exist only in minimal mode, because the full TUI solves the same problem with a pane or a chord.
    /// Clipboard helpers like `/copy` stay `Both`: they read scrollback state and do not need the fullscreen pane.
    fn mode_support(&self) -> ModeSupport {
        ModeSupport::Both
    }

    /// Placeholder text shown in the prompt when args are empty.
    /// E.g., `"[context]"` for `/compact`.
    fn arg_placeholder(&self) -> Option<&str> {
        None
    }

    /// Whether this command is a skill (ACP-advertised with skill metadata).
    /// Used for visual theming (accent color, prefix glyph).
    fn is_skill(&self) -> bool {
        false
    }

    /// Tool names the agent must have registered for this command to work. Override for commands that only make sense
    /// when specific tools are available. The registry hides commands whose requirements aren't all present in the
    /// agent's advertised toolset.
    fn required_tools(&self) -> &[&str] {
        &[]
    }

    /// Whether this command supports live preview when navigating arg suggestions in the dropdown.
    ///
    /// When true, [`preview_arg`] is called on every selection change and [`cancel_preview`] on dropdown close (Esc).
    fn supports_preview(&self) -> bool {
        false
    }

    /// Capture the current preview-relevant state as a string.
    /// Called once when preview mode begins (first navigation in args dropdown).
    /// The returned value is stored and passed back to [`cancel_preview`] if the user dismisses the dropdown.
    fn preview_state(&self) -> Option<String> {
        None
    }

    /// Live-preview the given argument suggestion. Only called when [`supports_preview`] returns true.
    #[allow(unused_variables)]
    fn preview_arg(&self, arg: &str) {}

    /// Cancel a live preview, reverting to the state before the dropdown opened. Only called when [`supports_preview`]
    /// returns true.
    #[allow(unused_variables)]
    fn cancel_preview(&self, previous: &str) {}

    /// Execute the command synchronously.
    ///
    /// For async work, return `CommandResult::Action(action)` and let the dispatch layer handle the effect pipeline.
    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult;
}

/// Every field is optional but must appear in the order below, the trait's declaration order. Only listed fields
/// emit an override, so omitted methods keep the trait default. `arg_placeholder` takes the bare placeholder text
/// (wrapped in `Some`), and `aliases` a bracketed list without the `&`.
macro_rules! slash_meta {
    (
        $(name: $name:expr,)?
        $(aliases: [$($alias:expr),+ $(,)?],)?
        $(description: $description:expr,)?
        $(usage: $usage:expr,)?
        $(takes_args: $takes_args:expr,)?
        $(args_required: $args_required:expr,)?
        $(session_scoped: $session_scoped:expr,)?
        $(offered_when_session_less: $offered_when_session_less:expr,)?
        $(dashboard_only: $dashboard_only:expr,)?
        $(can_hoist_from_mid_text: $can_hoist_from_mid_text:expr,)?
        $(mode_support: $mode_support:expr,)?
        $(arg_placeholder: $arg_placeholder:expr,)?
        $(required_tools: $required_tools:expr,)?
    ) => {
        $(fn name(&self) -> &str {
            $name
        })?

        $(fn aliases(&self) -> &[&str] {
            &[$($alias),+]
        })?

        $(fn description(&self) -> &str {
            $description
        })?

        $(fn usage(&self) -> &str {
            $usage
        })?

        $(fn takes_args(&self) -> bool {
            $takes_args
        })?

        $(fn args_required(&self) -> bool {
            $args_required
        })?

        $(fn session_scoped(&self) -> bool {
            $session_scoped
        })?

        $(fn offered_when_session_less(&self) -> bool {
            $offered_when_session_less
        })?

        $(fn dashboard_only(&self) -> bool {
            $dashboard_only
        })?

        $(fn can_hoist_from_mid_text(&self) -> bool {
            $can_hoist_from_mid_text
        })?

        $(fn mode_support(&self) -> crate::slash::mode_support::ModeSupport {
            $mode_support
        })?

        $(fn arg_placeholder(&self) -> Option<&str> {
            Some($arg_placeholder)
        })?

        $(fn required_tools(&self) -> &[&str] {
            $required_tools
        })?
    };
}

pub(crate) use slash_meta;
