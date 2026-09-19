// Modified for Distill by Samuel Fajreldines, 2026.
pub mod acp_types;
pub mod announcement_state;
pub(crate) mod auto_mode;
pub mod commands;
pub(crate) mod compaction_config;
pub(crate) mod doom_loop_telemetry;
pub(crate) mod fork_status;
pub mod handle;
pub(crate) mod memory_state;
pub mod merge;
pub(crate) mod message_delivery;
pub mod notifications;
pub mod pending_interaction;
pub mod prompt_queue;
pub(crate) mod resume_status;
pub mod two_pass;
pub mod user_echo;
pub mod visibility;
pub use self::acp_session::*;
pub use self::acp_types::*;
pub use self::commands::*;
pub use self::fork::{ForkSessionRequest, ForkSessionResponse, fork_session};
pub use self::handle::*;
pub use self::persistence::{
    LocalFeedbackEntry, UserFeedbackEntry, find_local_child_for_remote, resolve_local_session,
    resolve_local_session_any_cwd, resolve_local_session_ids_any_cwd, session_exists_for_cwd,
};
pub use self::result::{Empty, ExtMethodResult};
pub use self::share::{ShareSessionRequest, ShareSessionResponse};
pub use self::user_echo::{CLIENT_USER_MESSAGE_ECHO_META, USER_MESSAGE_ECHO_CAPABILITY};
pub use distill_fsnotify::{
    FsConfig, FsEvent, FsEventKind, FsEventSource, FsNotifyError, GitMetaKind,
};
pub use prod_mc_cli_chat_proxy_types::feedback_types::{
    ClientType, FeedbackImage, FeedbackTerminalInfo, MAX_FEEDBACK_IMAGE_BYTES,
    MAX_FEEDBACK_IMAGE_TOTAL_BYTES, MAX_FEEDBACK_IMAGES, RatingType, feedback_image_extension,
    validate_feedback_images,
};
/// `false` twin: this template is not compiled into this build, so no template matches.
/// Keeps ungated call sites compiling in both configurations.
pub(crate) fn is_cursor_user_template(
    _template: &distill_agent::prompt::user_message::UserMessageTemplate,
) -> bool {
    false
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CompactionPins {
    pub mode: distill_chat_state::CompactionMode,
    pub two_pass: bool,
}
pub(crate) fn cursor_compaction_pins(
    resolved_mode: distill_chat_state::CompactionMode,
    resolved_two_pass: bool,
    is_cursor: bool,
) -> CompactionPins {
    if is_cursor {
        CompactionPins {
            mode: distill_chat_state::CompactionMode::Summary,
            two_pass: false,
        }
    } else {
        CompactionPins {
            mode: resolved_mode,
            two_pass: resolved_two_pass,
        }
    }
}
/// `false` twin of [`is_cursor_system_template`]; see [`is_cursor_user_template`].
pub(crate) fn is_cursor_system_template(
    _template: &distill_agent::prompt::context::TemplateOverride,
) -> bool {
    false
}
/// The single spelling of "only Image blocks are carried structurally" (interject parse and queue-interject harvest).
pub(crate) fn image_blocks(
    blocks: impl IntoIterator<Item = agent_client_protocol::ContentBlock>,
) -> Vec<agent_client_protocol::ImageContent> {
    blocks
        .into_iter()
        .filter_map(|block| match block {
            agent_client_protocol::ContentBlock::Image(img) => Some(img),
            _ => None,
        })
        .collect()
}
pub use distill_agent_lifecycle::{
    AnalyticsClass, CompactionClass, InputAuthority, InputPolicy, QueuePolicy, ShutdownPolicy,
    SlashAuthority, TurnBoundary,
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptOrigin {
    User,
    /// Auto-wake prompt injected when a background terminal task completed.
    TaskCompleted {
        /// The background task ID (without the `task-completed-` prefix).
        task_id: String,
    },
    /// Auto-wake prompt injected when a background subagent completed.
    SubagentCompleted {
        /// The subagent ID (without the `subagent-completed-` prefix).
        subagent_id: String,
    },
    /// Model-authored context from the owning root session.
    ParentAgentMessage {
        message_id: String,
        sender_session_id: String,
    },
    /// Human text from the owning parent. Slash-inert; `@file` stays closed.
    ParentHumanMessage {
        message_id: String,
        sender_session_id: String,
    },
    WorkflowCompleted {
        completion_id: String,
    },
    /// Server-initiated prompt from the idle-gated notification drain (`maybe_drain_notifications`).
    /// Batches one or more monitor-event or bash-task-completed notifications into a single turn while the user is idle.
    NotificationDrain,
    /// The goal orchestrator injects a system reminder into context and then triggers a model turn so the model can print a visible progress update.
    GoalSummary,
    /// Nudge injected when the verification stage rejects an `update_goal(completed: true)` attempt.
    /// Carries the "not yet achieved — keep working" system-reminder body alongside the path to the persisted details file.
    /// The variant name retains the `Classifier` prefix for wire stability.
    GoalClassifierNudge,
    /// Scheduled task (`/loop`) prompt fired by the scheduler via the pager.
    SchedulerFired,
    /// The shell re-parked `exit_plan_mode` on resume, the user approved/revised, and the shell injects the follow-up turn.
    /// Synthetic: the user never typed it, so it stays out of prompt history, but it still runs a real turn.
    PlanResume,
}
impl PromptOrigin {
    pub fn from_prompt_id(prompt_id: &str) -> Self {
        if let Some(task_id) = prompt_id.strip_prefix("task-completed-") {
            Self::TaskCompleted {
                task_id: task_id.to_string(),
            }
        } else if let Some(subagent_id) = prompt_id.strip_prefix("subagent-completed-") {
            Self::SubagentCompleted {
                subagent_id: subagent_id.to_string(),
            }
        } else if let Some(parent_message_id) = prompt_id.strip_prefix("parent-agent-message-") {
            Self::ParentAgentMessage {
                message_id: parent_message_id.to_string(),
                sender_session_id: String::new(),
            }
        } else if let Some(parent_message_id) = prompt_id.strip_prefix("parent-message-") {
            Self::ParentHumanMessage {
                message_id: parent_message_id.to_string(),
                sender_session_id: String::new(),
            }
        } else if let Some(completion_id) = prompt_id.strip_prefix("workflow-completed-") {
            Self::WorkflowCompleted {
                completion_id: completion_id.to_string(),
            }
        } else if prompt_id.starts_with("notifications-") {
            Self::NotificationDrain
        } else if prompt_id.starts_with("goal-summary-") {
            Self::GoalSummary
        } else if prompt_id.starts_with("goal-classifier-nudge-") {
            Self::GoalClassifierNudge
        } else if prompt_id.starts_with("scheduler-fired-") {
            Self::SchedulerFired
        } else if prompt_id.starts_with("plan-resume-") {
            Self::PlanResume
        } else {
            Self::User
        }
    }
    pub const fn policy(&self) -> InputPolicy {
        match self {
            Self::User => InputPolicy {
                authority: InputAuthority::HumanIntent,
                slash: SlashAuthority::HumanCatalog,
                turn_boundary: TurnBoundary::Conversational,
                analytics: AnalyticsClass::HumanPrompt,
                compaction: CompactionClass::HumanAnchor,
                queue: QueuePolicy::VisibleEditable,
                shutdown: ShutdownPolicy::Drain,
            },
            Self::ParentAgentMessage { .. } => InputPolicy {
                authority: InputAuthority::ModelAuthoredUntrusted,
                slash: SlashAuthority::ModelAuthored,
                turn_boundary: TurnBoundary::Conversational,
                analytics: AnalyticsClass::AgentMessage,
                compaction: CompactionClass::ConversationalAgentAnchor,
                queue: QueuePolicy::VisibleProtected,
                shutdown: ShutdownPolicy::Drain,
            },
            Self::ParentHumanMessage { .. } => InputPolicy {
                authority: InputAuthority::ModelAuthoredUntrusted,
                slash: SlashAuthority::Inert,
                turn_boundary: TurnBoundary::Conversational,
                analytics: AnalyticsClass::AgentMessage,
                compaction: CompactionClass::ConversationalAgentAnchor,
                queue: QueuePolicy::VisibleProtected,
                shutdown: ShutdownPolicy::Drain,
            },
            Self::TaskCompleted { .. }
            | Self::SubagentCompleted { .. }
            | Self::WorkflowCompleted { .. }
            | Self::SchedulerFired => InputPolicy {
                authority: InputAuthority::RuntimeControl,
                slash: SlashAuthority::Inert,
                turn_boundary: TurnBoundary::Conversational,
                analytics: AnalyticsClass::RuntimeWake,
                compaction: CompactionClass::RuntimeEphemera,
                queue: QueuePolicy::Hidden,
                shutdown: ShutdownPolicy::CancelWithProducer,
            },
            Self::NotificationDrain
            | Self::GoalSummary
            | Self::GoalClassifierNudge
            | Self::PlanResume => InputPolicy {
                authority: InputAuthority::RuntimeControl,
                slash: SlashAuthority::Inert,
                turn_boundary: TurnBoundary::Conversational,
                analytics: AnalyticsClass::RuntimeWake,
                compaction: CompactionClass::RuntimeEphemera,
                queue: QueuePolicy::Hidden,
                shutdown: ShutdownPolicy::DropEphemeral,
            },
        }
    }
    /// Returns `true` for auto-wake (synthetic) prompts.
    pub fn is_synthetic(&self) -> bool {
        !matches!(self, Self::User)
    }
    /// A queued user follow-up must wait for these to finish; Steer must not
    /// promote into them.
    pub fn is_auto_wake(&self) -> bool {
        matches!(
            self,
            Self::TaskCompleted { .. }
                | Self::SubagentCompleted { .. }
                | Self::WorkflowCompleted { .. }
                | Self::ParentAgentMessage { .. }
                | Self::ParentHumanMessage { .. }
                | Self::NotificationDrain
        )
    }
    /// Whether a `UserMessageChunk` echo for this origin must stay out of client scrollback (live and on resume).
    /// The hidden origins carry model-only, side-channel content the UI already shows elsewhere (task pane, monitor gutter, etc.).
    pub fn hide_user_echo_from_scrollback(&self) -> bool {
        match self {
            Self::User
            | Self::ParentAgentMessage { .. }
            | Self::ParentHumanMessage { .. }
            | Self::SchedulerFired
            | Self::PlanResume => false,
            Self::TaskCompleted { .. }
            | Self::SubagentCompleted { .. }
            | Self::WorkflowCompleted { .. }
            | Self::NotificationDrain
            | Self::GoalSummary
            | Self::GoalClassifierNudge => true,
        }
    }
    pub fn completion_id(&self) -> Option<&str> {
        match self {
            Self::TaskCompleted { task_id } => Some(task_id),
            Self::SubagentCompleted { subagent_id } => Some(subagent_id),
            Self::WorkflowCompleted { completion_id } => Some(completion_id),
            Self::User
            | Self::ParentAgentMessage { .. }
            | Self::ParentHumanMessage { .. }
            | Self::NotificationDrain
            | Self::GoalSummary
            | Self::GoalClassifierNudge
            | Self::SchedulerFired
            | Self::PlanResume => None,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::{PromptOrigin, QueuePolicy};
    #[test]
    fn from_prompt_id_user() {
        assert_eq!(
            PromptOrigin::from_prompt_id("my-prompt"),
            PromptOrigin::User
        );
        assert!(!PromptOrigin::from_prompt_id("my-prompt").is_synthetic());
    }
    #[test]
    fn from_prompt_id_task_completed() {
        let origin = PromptOrigin::from_prompt_id("task-completed-abc-123");
        assert_eq!(
            origin,
            PromptOrigin::TaskCompleted {
                task_id: "abc-123".into()
            }
        );
        assert!(origin.is_synthetic());
        assert!(origin.is_auto_wake());
        assert_eq!(origin.completion_id(), Some("abc-123"));
        assert!(!PromptOrigin::from_prompt_id("my-prompt").is_auto_wake());
    }
    #[test]
    fn from_prompt_id_parent_message() {
        let origin = PromptOrigin::from_prompt_id("parent-message-msg-123");
        assert_eq!(
            origin,
            PromptOrigin::ParentHumanMessage {
                message_id: "msg-123".into(),
                sender_session_id: String::new(),
            }
        );
        assert_eq!(origin.policy().slash, crate::session::SlashAuthority::Inert);
        assert!(origin.is_synthetic());
    }
    #[test]
    fn wake_compact_slash_preserves_parent_authority() {
        for (prompt_id, expected, slash) in [
            (
                "parent-agent-message-wake",
                PromptOrigin::ParentAgentMessage {
                    message_id: "wake".into(),
                    sender_session_id: String::new(),
                },
                crate::session::SlashAuthority::ModelAuthored,
            ),
            (
                "parent-message-wake",
                PromptOrigin::ParentHumanMessage {
                    message_id: "wake".into(),
                    sender_session_id: String::new(),
                },
                crate::session::SlashAuthority::Inert,
            ),
        ] {
            let origin = PromptOrigin::from_prompt_id(prompt_id);
            assert_eq!(origin, expected);
            assert_ne!(
                origin.policy().authority,
                crate::session::InputAuthority::HumanIntent,
                "/compact must not enter the human command path",
            );
            assert_eq!(origin.policy().slash, slash);
        }
    }
    #[test]
    fn from_prompt_id_subagent_completed() {
        let origin = PromptOrigin::from_prompt_id("subagent-completed-xyz-789");
        assert_eq!(
            origin,
            PromptOrigin::SubagentCompleted {
                subagent_id: "xyz-789".into()
            }
        );
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), Some("xyz-789"));
    }
    #[test]
    fn from_prompt_id_notification_drain() {
        let origin =
            PromptOrigin::from_prompt_id("notifications-019e0000-0000-7000-8000-0000000000aa");
        assert_eq!(origin, PromptOrigin::NotificationDrain);
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), None);
    }
    #[test]
    fn goal_summary_origin_from_prompt_id() {
        let origin = PromptOrigin::from_prompt_id("goal-summary-019e2d3e");
        assert!(matches!(origin, PromptOrigin::GoalSummary));
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), None);
    }
    #[test]
    fn goal_classifier_nudge_origin_from_prompt_id() {
        let origin = PromptOrigin::from_prompt_id("goal-classifier-nudge-019e2d3e");
        assert!(matches!(origin, PromptOrigin::GoalClassifierNudge));
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), None);
    }
    #[test]
    fn goal_classifier_nudge_origin_round_trips_through_from_prompt_id() {
        let prompt_id = format!("goal-classifier-nudge-{}", uuid::Uuid::now_v7());
        let origin = PromptOrigin::from_prompt_id(&prompt_id);
        assert!(matches!(origin, PromptOrigin::GoalClassifierNudge));
        assert!(origin.is_synthetic());
    }
    #[test]
    fn scheduler_fired_origin_from_prompt_id() {
        let origin = PromptOrigin::from_prompt_id("scheduler-fired-019e51a3-abcd-1234");
        assert!(matches!(origin, PromptOrigin::SchedulerFired));
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), None);
    }
    #[test]
    fn plan_resume_origin_from_prompt_id() {
        let origin = PromptOrigin::from_prompt_id("plan-resume-1730000000000");
        assert!(matches!(origin, PromptOrigin::PlanResume));
        assert!(origin.is_synthetic());
        assert_eq!(origin.completion_id(), None);
    }
    #[test]
    fn notification_drain_is_server_initiated() {
        let prompt_id = "notifications-019e0000-0000-7000-8000-0000000000aa";
        assert!(PromptOrigin::from_prompt_id(prompt_id).is_synthetic());
    }
    #[test]
    fn current_origin_queue_policies_are_preserved() {
        let cases = [
            (PromptOrigin::User, QueuePolicy::VisibleEditable),
            (
                PromptOrigin::TaskCompleted {
                    task_id: "t".into(),
                },
                QueuePolicy::Hidden,
            ),
            (
                PromptOrigin::SubagentCompleted {
                    subagent_id: "s".into(),
                },
                QueuePolicy::Hidden,
            ),
            (
                PromptOrigin::ParentAgentMessage {
                    message_id: "m".into(),
                    sender_session_id: "root".into(),
                },
                QueuePolicy::VisibleProtected,
            ),
            (
                PromptOrigin::ParentHumanMessage {
                    message_id: "h".into(),
                    sender_session_id: "root".into(),
                },
                QueuePolicy::VisibleProtected,
            ),
            (
                PromptOrigin::WorkflowCompleted {
                    completion_id: "w".into(),
                },
                QueuePolicy::Hidden,
            ),
            (PromptOrigin::NotificationDrain, QueuePolicy::Hidden),
            (PromptOrigin::GoalSummary, QueuePolicy::Hidden),
            (PromptOrigin::GoalClassifierNudge, QueuePolicy::Hidden),
            (PromptOrigin::SchedulerFired, QueuePolicy::Hidden),
            (PromptOrigin::PlanResume, QueuePolicy::Hidden),
        ];
        for (origin, queue) in cases {
            assert_eq!(origin.policy().queue, queue, "{origin:?}");
        }
    }
    #[test]
    fn hide_user_echo_from_scrollback_by_origin() {
        assert!(!PromptOrigin::User.hide_user_echo_from_scrollback());
        assert!(
            !PromptOrigin::ParentAgentMessage {
                message_id: "m".into(),
                sender_session_id: "root".into(),
            }
            .hide_user_echo_from_scrollback()
        );
        assert!(
            !PromptOrigin::ParentHumanMessage {
                message_id: "h".into(),
                sender_session_id: "root".into(),
            }
            .hide_user_echo_from_scrollback()
        );
        assert!(
            !PromptOrigin::from_prompt_id("scheduler-fired-abc").hide_user_echo_from_scrollback()
        );
        assert!(!PromptOrigin::from_prompt_id("plan-resume-1").hide_user_echo_from_scrollback());
        assert!(PromptOrigin::from_prompt_id("task-completed-t1").hide_user_echo_from_scrollback());
        assert!(
            PromptOrigin::from_prompt_id("subagent-completed-s1").hide_user_echo_from_scrollback()
        );
        assert!(
            PromptOrigin::from_prompt_id("notifications-uuid").hide_user_echo_from_scrollback()
        );
        assert!(
            PromptOrigin::from_prompt_id("workflow-completed-wf-1-9")
                .hide_user_echo_from_scrollback()
        );
        assert!(PromptOrigin::from_prompt_id("goal-summary-1").hide_user_echo_from_scrollback());
        assert!(
            PromptOrigin::from_prompt_id("goal-classifier-nudge-1")
                .hide_user_echo_from_scrollback()
        );
    }
}
/// Determines whether the session sends an initial file index to the client or just streams raw file events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub(crate) enum ClientFsMode {
    #[default]
    Events,
    Index,
}
#[derive(Debug, Clone, Default)]
pub(crate) struct ClientFsConfig {
    pub fs: FsConfig,
    pub mode: ClientFsMode,
}
pub mod share {
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct ShareSessionRequest {
        pub session_id: String,
    }
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct ShareSessionResponse {
        pub share_url: String,
    }
}
/// Proxy config for the session registry client.
/// Shared between `acp_session` (slash commands) and `persistence` (title generation).
#[derive(Clone)]
pub(crate) struct RegistryConfig {
    pub base_url: String,
    pub user_token: String,
    pub deployment_key: Option<String>,
    pub alpha_test_key: Option<String>,
}
pub mod acp_conversion;
pub(crate) mod acp_mcp;
pub(crate) mod acp_session;
pub(crate) mod agent_rebuild;
pub(crate) mod chat_persistence;
pub(crate) mod events;
pub mod export;
pub mod feedback;
pub mod feedback_manager;
pub mod file_system;
pub mod fork;
pub(crate) mod fs_watch;
pub(crate) mod goal_classifier;
pub(crate) mod goal_evaluator;
pub(crate) mod goal_next_step;
pub(crate) mod goal_orchestrator;
pub(crate) mod goal_planner;
pub(crate) mod goal_role_tools;
pub(crate) mod goal_stop_detector;
pub(crate) mod goal_strategist;
pub(crate) mod goal_summarizer;
pub mod goal_tracker;
pub mod helpers;
pub(crate) mod image_describe;
pub(crate) mod image_normalize;
pub(crate) mod inference_metrics;
pub use distill_shared::session::info;
pub mod managed_mcp;
pub(crate) mod mcp_descriptors;
pub(crate) mod mcp_dispatcher;
#[cfg(test)]
mod mcp_dispatcher_e2e_tests;
pub(crate) mod mcp_elicitation;
pub(crate) mod mcp_restart;
pub mod mcp_servers;
pub mod memory;
pub(crate) mod memory_observation;
pub(crate) mod normalize_cache;
pub mod persistence;
pub(crate) mod session_create_prefetch;
pub use distill_shared::placeholder_images;
pub mod plan_mode;
pub mod prompt_history;
pub mod prompt_parser;
pub(crate) mod prompt_timing;
pub(crate) mod replay_events;
pub mod repo_changes;
#[path = "restore_stub.rs"]
pub mod restore;
pub mod result;
pub mod signals;
pub(crate) mod slash_authority;
pub(crate) mod slash_commands;
pub mod usage_file;
pub use slash_commands::PAGER_COMMAND_KEYS;
pub(crate) mod repo_status_prefix;
pub mod storage;
pub(crate) mod streaming_capture;
pub(crate) mod summary;
pub(crate) mod telemetry;
#[cfg(feature = "test-support")]
pub mod testkit;
pub mod tool_definitions_artifact;
pub mod tool_index;
pub mod turn_completion;
pub mod unified_list;
pub(crate) mod user_message;
pub(crate) mod wire_tags;
pub(crate) mod workflow;
pub mod worktree;
pub(crate) mod worktree_cleanup;
pub mod worktree_pool;
