use serde::Serialize;

pub const MAX_PAYLOAD_SIZE: usize = 128 * 1024;

macro_rules! hook_events {
    ($(
        $(#[$vmeta:meta])*
        $variant:ident {
            display: $display:literal,
            aliases: [$first_alias:literal $(, $alias:literal)* $(,)?],
            traits: ($gate:ident, $matcher:ident, $hub:literal $(,)?),
        }
    ),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum HookEventName {
            $($(#[$vmeta])* $variant),*
        }

        impl HookEventName {
            pub const ALL: &'static [HookEventName] = &[$(HookEventName::$variant),*];

            fn from_key_str(s: &str) -> Option<Self> {
                match s {
                    $($first_alias $(| $alias)* => Some(Self::$variant),)*
                    _ => None,
                }
            }

            pub fn pascal_case(self) -> &'static str {
                match self { $(Self::$variant => $first_alias,)* }
            }

            pub fn traits(self) -> EventTraits {
                use GateKind::*;
                use MatcherPolicy::*;
                match self {
                    $(Self::$variant => EventTraits {
                        gate: $gate,
                        matcher: $matcher,
                        hub_forward: $hub,
                    },)*
                }
            }
        }

        impl std::fmt::Display for HookEventName {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(match self { $(Self::$variant => $display,)* })
            }
        }

        impl<'de> serde::Deserialize<'de> for HookEventName {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let s = <String as serde::Deserialize>::deserialize(deserializer)?;
                Self::from_key_str(&s).ok_or_else(|| {
                    let known = Self::ALL
                        .iter()
                        .map(|e| e.to_string())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect::<Vec<_>>()
                        .join(", ");
                    serde::de::Error::custom(format!(
                        "unknown hook event: '{s}'. Expected one of: {known} \
                         (camelCase and per-operation aliases such as \
                         beforeShellExecution are also accepted)"
                    ))
                })
            }
        }
    };
}

hook_events! {
    SessionStart {
        display: "session_start",
        aliases: ["SessionStart", "session_start", "sessionStart"],
        traits: (Observe, Tested, true),
    },
    UserPromptSubmit {
        display: "user_prompt_submit",
        aliases: ["UserPromptSubmit", "user_prompt_submit", "beforeSubmitPrompt"],
        traits: (Prompt, Ignored, true),
    },
    PreToolUse {
        display: "pre_tool_use",
        aliases: [
            "PreToolUse",
            "pre_tool_use",
            "preToolUse",
            "beforeShellExecution",
            "beforeMCPExecution",
            "beforeReadFile",
        ],
        traits: (Tool, Tested, false),
    },
    PostToolUse {
        display: "post_tool_use",
        aliases: [
            "PostToolUse",
            "post_tool_use",
            "postToolUse",
            "afterShellExecution",
            "afterMCPExecution",
            "afterFileEdit",
            "afterAgentResponse",
            "afterAgentThought",
        ],
        traits: (PostTool, Tested, true),
    },
    PostToolUseFailure {
        display: "post_tool_use_failure",
        aliases: ["PostToolUseFailure", "post_tool_use_failure", "postToolUseFailure"],
        traits: (Observe, Tested, true),
    },
    PermissionDenied {
        display: "permission_denied",
        aliases: ["PermissionDenied", "permission_denied", "permissionDenied"],
        traits: (Observe, Tested, true),
    },
    Stop {
        display: "stop",
        aliases: ["Stop", "stop"],
        traits: (Stop, Ignored, true),
    },
    StopFailure {
        display: "stop_failure",
        aliases: ["StopFailure", "stop_failure", "stopFailure"],
        traits: (Observe, Tested, true),
    },
    StopCancelled {
        display: "stop_cancelled",
        aliases: [
            "StopCancelled",
            "stop_cancelled",
            "stopCancelled",
        ],
        traits: (Observe, Tested, true),
    },
    Notification {
        display: "notification",
        aliases: ["Notification", "notification"],
        traits: (Observe, Tested, true),
    },
    SubagentStart {
        display: "subagent_start",
        aliases: ["SubagentStart", "subagent_start", "subagentStart"],
        traits: (Observe, Tested, true),
    },
    SubagentStop {
        display: "subagent_stop",
        aliases: ["SubagentStop", "subagent_stop", "subagentStop"],
        traits: (Stop, Tested, true),
    },
    /// Legacy alias of `SubagentStop`, collapsed by [`HookEventName::canonical`].
    SubagentEnd {
        display: "subagent_stop",
        aliases: ["SubagentEnd", "subagent_end", "subagentEnd"],
        traits: (Stop, Tested, true),
    },
    PreCompact {
        display: "pre_compact",
        aliases: ["PreCompact", "pre_compact", "preCompact"],
        traits: (Observe, Tested, true),
    },
    PostCompact {
        display: "post_compact",
        aliases: ["PostCompact", "post_compact", "postCompact"],
        traits: (Observe, Tested, true),
    },
    SessionEnd {
        display: "session_end",
        aliases: ["SessionEnd", "session_end", "sessionEnd"],
        traits: (Observe, Tested, true),
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateKind {
    Observe,
    Tool,
    Stop,
    PostTool,
    /// Prompt decision control (`decision: "block"` with `reason`, exit 2).
    /// The block reason is user-facing, never model context.
    /// Exit 2 blocks regardless of JSON, and the default timeout is 30s.
    Prompt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatcherPolicy {
    Ignored,
    Tested,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventTraits {
    pub gate: GateKind,
    pub matcher: MatcherPolicy,
    pub hub_forward: bool,
}

impl HookEventName {
    pub fn canonical(self) -> Self {
        match self {
            Self::SubagentEnd => Self::SubagentStop,
            other => other,
        }
    }

    pub fn parse_key(s: &str) -> Option<Self> {
        Self::from_key_str(s)
    }
}

pub const MAX_STOP_ENTRY_TEXT_CHARS: usize = 1000;

pub const MAX_CANCEL_TRIGGER_CHARS: usize = 64;

pub const MAX_ASSISTANT_MESSAGE_CHARS: usize = 32_768;

pub fn clip_assistant_message(text: &str) -> String {
    clip_text(text, MAX_ASSISTANT_MESSAGE_CHARS)
}

pub fn clip_text(text: &str, max: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= max {
        return text.to_string();
    }
    let clipped: String = text.chars().take(max).collect();
    format!("{clipped}… [+{} chars]", char_count - max)
}

pub fn clip_stop_entry_text(text: &str) -> String {
    clip_text(text, MAX_STOP_ENTRY_TEXT_CHARS)
}

// Cap on hook-influenced strings so one huge line can't flood the model or logs.
pub const MAX_REASON_CHARS: usize = 256;

pub const MAX_HOOK_FEEDBACK_CHARS: usize = 10_000;

pub const MAX_HOOK_OUTPUT_REPLACEMENT_CHARS: usize = 64 * 1024;

pub fn clip_reason(reason: &str) -> String {
    clip_text(reason, MAX_REASON_CHARS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentStopPhase {
    Gate,
    Observe,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopBackgroundTask {
    pub id: String,
    pub r#type: BackgroundTaskType,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopSessionCron {
    pub id: String,
    pub schedule: String,
    pub recurring: bool,
    pub prompt: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskType {
    Shell,
    Monitor,
    Subagent,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    strum::AsRefStr,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum StopFailureKind {
    RateLimit,
    AuthenticationFailed,
    InvalidRequest,
    ServerError,
    MaxOutputTokens,
    Unknown,
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    strum::AsRefStr,
    strum::IntoStaticStr,
    strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum StopCancelledReason {
    UserInterrupt,
    PermissionRejected,
    PermissionCancelled,
    MaxTurns,
    NoProgress,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelledBy {
    User,
    Runtime,
    Unknown,
}

impl StopCancelledReason {
    pub fn cancelled_by(self) -> CancelledBy {
        match self {
            Self::UserInterrupt | Self::PermissionRejected | Self::PermissionCancelled => {
                CancelledBy::User
            }
            Self::MaxTurns | Self::NoProgress => CancelledBy::Runtime,
            Self::Unknown => CancelledBy::Unknown,
        }
    }
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookEventEnvelope {
    pub hook_event_name: HookEventName,
    pub session_id: String,
    pub cwd: String,
    pub workspace_root: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_identifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(flatten)]
    pub payload: HookPayload,
}

// Additive snake_case aliases for grok's camelCase keys, which some hook clients read; the camelCase keys stay authoritative
const SNAKE_CASE_ALIASES: &[(&str, &str)] = &[
    ("hookEventName", "hook_event_name"),
    ("sessionId", "session_id"),
    ("transcriptPath", "transcript_path"),
    ("permissionMode", "permission_mode"),
    ("toolName", "tool_name"),
    ("toolInput", "tool_input"),
    ("toolResult", "tool_response"),
    ("toolUseId", "tool_use_id"),
    ("durationMs", "duration_ms"),
    ("isInterrupt", "is_interrupt"),
];

impl HookEventEnvelope {
    pub fn to_hook_json(&self) -> serde_json::Value {
        let mut value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let serde_json::Value::Object(map) = &mut value {
            for (camel, snake) in SNAKE_CASE_ALIASES {
                if let Some(aliased) = map.get(*camel).cloned() {
                    map.entry(*snake).or_insert(aliased);
                }
            }
            // The snake key carries Claude's PascalCase value; the camel key stays grok-native.
            map.insert(
                "hook_event_name".to_string(),
                self.hook_event_name.pascal_case().into(),
            );
        }
        value
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum HookPayload {
    SessionStart {
        source: String,
        #[serde(rename = "modelId", skip_serializing_if = "Option::is_none")]
        model_id: Option<String>,
        #[serde(rename = "agentType", skip_serializing_if = "Option::is_none")]
        agent_type: Option<String>,
    },
    SessionEnd {
        reason: String,
        #[serde(rename = "turnCount", skip_serializing_if = "Option::is_none")]
        turn_count: Option<u64>,
        #[serde(rename = "toolCallCount", skip_serializing_if = "Option::is_none")]
        tool_call_count: Option<u64>,
        #[serde(rename = "subagentType", skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },
    Stop {
        reason: String,
        #[serde(rename = "stopHookActive")]
        stop_hook_active: bool,
        #[serde(
            rename = "lastAssistantMessage",
            skip_serializing_if = "Option::is_none"
        )]
        last_assistant_message: Option<String>,
        #[serde(rename = "backgroundTasks", skip_serializing_if = "Option::is_none")]
        background_tasks: Option<Vec<StopBackgroundTask>>,
        #[serde(rename = "sessionCrons", skip_serializing_if = "Option::is_none")]
        session_crons: Option<Vec<StopSessionCron>>,
    },
    StopFailure {
        error: StopFailureKind,
        #[serde(rename = "errorDetails", skip_serializing_if = "Option::is_none")]
        error_details: Option<String>,
        #[serde(
            rename = "lastAssistantMessage",
            skip_serializing_if = "Option::is_none"
        )]
        last_assistant_message: Option<String>,
        #[serde(rename = "subagentType", skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },
    StopCancelled {
        reason: StopCancelledReason,
        #[serde(rename = "cancelledBy")]
        cancelled_by: CancelledBy,
        #[serde(rename = "cancelTrigger", skip_serializing_if = "Option::is_none")]
        cancel_trigger: Option<String>,
        #[serde(rename = "reasonDetails", skip_serializing_if = "Option::is_none")]
        reason_details: Option<String>,
        #[serde(
            rename = "lastAssistantMessage",
            skip_serializing_if = "Option::is_none"
        )]
        last_assistant_message: Option<String>,
        #[serde(rename = "subagentType", skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },

    PreToolUse {
        /// For meta-dispatch tools (`use_tool`, the external MCP-call tool) this is the underlying tool (`server__tool`), not the dispatcher.
        /// Matchers key on the real target.
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        #[serde(rename = "toolInput")]
        tool_input: serde_json::Value,
        #[serde(rename = "toolInputTruncated")]
        tool_input_truncated: bool,
        #[serde(rename = "subagentType", skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },
    PostToolUse {
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        #[serde(rename = "toolInput")]
        tool_input: serde_json::Value,
        #[serde(rename = "toolResult")]
        tool_result: serde_json::Value,
        #[serde(rename = "toolInputTruncated")]
        tool_input_truncated: bool,
        #[serde(rename = "toolResultTruncated")]
        tool_result_truncated: bool,
        #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(rename = "isBackgrounded")]
        is_backgrounded: bool,
        #[serde(rename = "subagentType", skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },
    PostToolUseFailure {
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        #[serde(rename = "toolInput")]
        tool_input: serde_json::Value,
        #[serde(rename = "toolInputTruncated")]
        tool_input_truncated: bool,
        error: String,
        #[serde(rename = "durationMs", skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
        #[serde(rename = "isInterrupt")]
        is_interrupt: bool,
        #[serde(rename = "subagentType", skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },
    PermissionDenied {
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(rename = "toolUseId")]
        tool_use_id: String,
        #[serde(rename = "toolInput")]
        tool_input: serde_json::Value,
        #[serde(rename = "toolInputTruncated")]
        tool_input_truncated: bool,
    },

    UserPromptSubmit {
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        #[serde(rename = "subagentType", skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
    },
    Notification {
        #[serde(rename = "notificationType")]
        notification_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        level: Option<String>,
    },

    SubagentStart {
        #[serde(rename = "subagentId")]
        subagent_id: String,
        #[serde(rename = "subagentType")]
        subagent_type: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    SubagentStop {
        phase: SubagentStopPhase,
        #[serde(rename = "subagentId")]
        subagent_id: String,
        #[serde(rename = "subagentType")]
        subagent_type: String,
        #[serde(rename = "stopHookActive", skip_serializing_if = "Option::is_none")]
        stop_hook_active: Option<bool>,
        #[serde(
            rename = "lastAssistantMessage",
            skip_serializing_if = "Option::is_none"
        )]
        last_assistant_message: Option<String>,
    },

    PreCompact {
        source: String,
    },
    PostCompact {
        source: String,
    },
}

impl HookPayload {
    /// The tool a tool-event payload is about; `None` for every other event.
    pub fn tool_name(&self) -> Option<&str> {
        match self {
            Self::PreToolUse { tool_name, .. }
            | Self::PostToolUse { tool_name, .. }
            | Self::PostToolUseFailure { tool_name, .. }
            | Self::PermissionDenied { tool_name, .. } => Some(tool_name),
            _ => None,
        }
    }

    pub fn match_value(&self) -> Option<&str> {
        let value = match self {
            Self::PreToolUse { tool_name, .. }
            | Self::PostToolUse { tool_name, .. }
            | Self::PostToolUseFailure { tool_name, .. }
            | Self::PermissionDenied { tool_name, .. } => tool_name,
            Self::Notification {
                notification_type, ..
            } => notification_type,
            Self::SubagentStart { subagent_type, .. }
            | Self::SubagentStop { subagent_type, .. } => subagent_type,
            Self::SessionStart { source, .. }
            | Self::PreCompact { source }
            | Self::PostCompact { source } => source,
            Self::SessionEnd { reason, .. } => reason,
            Self::StopFailure { error, .. } => return Some(error.as_ref()),
            Self::StopCancelled { reason, .. } => return Some(reason.as_ref()),
            Self::Stop { .. } | Self::UserPromptSubmit { .. } => return None,
        };
        Some(value.as_str()).filter(|v| !v.is_empty())
    }
}

pub fn truncate_payload(value: serde_json::Value) -> (serde_json::Value, bool) {
    let serialized = serde_json::to_string(&value).unwrap_or_default();
    if serialized.len() <= MAX_PAYLOAD_SIZE {
        return (value, false);
    }

    // Cut on a char boundary so the slice never splits a multibyte codepoint.
    let mut end = MAX_PAYLOAD_SIZE;
    while !serialized.is_char_boundary(end) {
        end -= 1;
    }
    let Some(prefix) = serialized.get(..end) else {
        return (value, false);
    };
    let mut result = prefix.to_string();
    result.push_str(" [truncated]");
    (serde_json::Value::String(result), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_name_deser_all_variants() {
        let cases: &[(&str, &str, HookEventName)] = &[
            ("SessionStart", "session_start", HookEventName::SessionStart),
            ("PreToolUse", "pre_tool_use", HookEventName::PreToolUse),
            ("PostToolUse", "post_tool_use", HookEventName::PostToolUse),
            (
                "PostToolUseFailure",
                "post_tool_use_failure",
                HookEventName::PostToolUseFailure,
            ),
            ("SessionEnd", "session_end", HookEventName::SessionEnd),
            ("Stop", "stop", HookEventName::Stop),
            ("StopFailure", "stop_failure", HookEventName::StopFailure),
            (
                "StopCancelled",
                "stop_cancelled",
                HookEventName::StopCancelled,
            ),
            ("Notification", "notification", HookEventName::Notification),
            (
                "UserPromptSubmit",
                "user_prompt_submit",
                HookEventName::UserPromptSubmit,
            ),
            (
                "PermissionDenied",
                "permission_denied",
                HookEventName::PermissionDenied,
            ),
            (
                "SubagentStart",
                "subagent_start",
                HookEventName::SubagentStart,
            ),
            ("SubagentStop", "subagent_stop", HookEventName::SubagentStop),
            ("SubagentEnd", "subagent_end", HookEventName::SubagentEnd),
            ("PreCompact", "pre_compact", HookEventName::PreCompact),
            ("PostCompact", "post_compact", HookEventName::PostCompact),
        ];

        for (pascal, snake, expected) in cases {
            let from_pascal: HookEventName =
                serde_json::from_str(&format!("\"{pascal}\"")).unwrap();
            assert_eq!(
                from_pascal, *expected,
                "PascalCase deser failed for {pascal}"
            );

            let from_snake: HookEventName = serde_json::from_str(&format!("\"{snake}\"")).unwrap();
            assert_eq!(from_snake, *expected, "snake_case deser failed for {snake}");
        }
    }

    #[test]
    fn event_name_deser_camel_and_operation_aliases() {
        let cases: &[(&str, HookEventName)] = &[
            ("sessionStart", HookEventName::SessionStart),
            ("preToolUse", HookEventName::PreToolUse),
            ("beforeShellExecution", HookEventName::PreToolUse),
            ("beforeMCPExecution", HookEventName::PreToolUse),
            ("beforeReadFile", HookEventName::PreToolUse),
            ("postToolUse", HookEventName::PostToolUse),
            ("afterShellExecution", HookEventName::PostToolUse),
            ("afterMCPExecution", HookEventName::PostToolUse),
            ("afterFileEdit", HookEventName::PostToolUse),
            ("afterAgentResponse", HookEventName::PostToolUse),
            ("afterAgentThought", HookEventName::PostToolUse),
            ("beforeSubmitPrompt", HookEventName::UserPromptSubmit),
            ("subagentStop", HookEventName::SubagentStop),
            ("subagentEnd", HookEventName::SubagentEnd),
            ("preCompact", HookEventName::PreCompact),
            ("stopFailure", HookEventName::StopFailure),
            ("stopCancelled", HookEventName::StopCancelled),
        ];
        for (spelling, expected) in cases {
            let parsed: HookEventName = serde_json::from_str(&format!("\"{spelling}\"")).unwrap();
            assert_eq!(parsed, *expected, "alias deser failed for {spelling}");
        }
    }

    #[test]
    fn event_name_unknown_rejected() {
        let result = serde_json::from_str::<HookEventName>("\"UnknownEvent\"");
        assert!(result.is_err());
    }

    #[test]
    fn pascal_case_is_the_pascal_alias() {
        for event in HookEventName::ALL {
            let pascal = event.pascal_case();
            let first = pascal.chars().next().expect("alias is non-empty");
            assert!(
                first.is_ascii_uppercase() && !pascal.contains('_'),
                "{event}: pascal_case() must be the PascalCase alias, got {pascal:?}"
            );
        }
    }

    #[test]
    fn event_traits_report_gate_matcher_and_hub_forward() {
        use super::{GateKind, MatcherPolicy};

        assert_eq!(HookEventName::PreToolUse.traits().gate, GateKind::Tool);
        assert_eq!(HookEventName::Stop.traits().gate, GateKind::Stop);
        assert_eq!(
            HookEventName::UserPromptSubmit.traits().gate,
            GateKind::Prompt
        );
        assert_eq!(HookEventName::SubagentStop.traits().gate, GateKind::Stop);
        assert_eq!(
            HookEventName::SubagentEnd.traits().gate,
            GateKind::Stop,
            "alias resolves through canonical()"
        );
        assert_eq!(HookEventName::PostToolUse.traits().gate, GateKind::PostTool);

        assert_eq!(HookEventName::Stop.traits().matcher, MatcherPolicy::Ignored);
        assert_eq!(
            HookEventName::UserPromptSubmit.traits().matcher,
            MatcherPolicy::Ignored
        );
        assert_eq!(
            HookEventName::SessionStart.traits().matcher,
            MatcherPolicy::Tested
        );

        assert!(!HookEventName::PreToolUse.traits().hub_forward);
        assert!(HookEventName::Stop.traits().hub_forward);
    }

    #[test]
    fn clip_stop_entry_text_clips_on_char_boundary() {
        assert_eq!(clip_stop_entry_text("short"), "short");
        let exact = "x".repeat(MAX_STOP_ENTRY_TEXT_CHARS);
        assert_eq!(clip_stop_entry_text(&exact), exact);

        let long = "x".repeat(MAX_STOP_ENTRY_TEXT_CHARS + 42);
        let clipped = clip_stop_entry_text(&long);
        assert!(clipped.ends_with("… [+42 chars]"));

        let unicode = "€".repeat(MAX_STOP_ENTRY_TEXT_CHARS + 7);
        let clipped = clip_stop_entry_text(&unicode);
        assert!(clipped.ends_with("… [+7 chars]"));
    }

    #[test]
    fn truncate_payload_respects_limit() {
        let small = serde_json::json!({"key": "small"});
        let (result, truncated) = truncate_payload(small.clone());
        assert!(!truncated);
        assert_eq!(result, small);

        let (result, truncated) = truncate_payload(serde_json::Value::String(
            "x".repeat(MAX_PAYLOAD_SIZE + 1000),
        ));
        assert!(truncated);
        let s = result.as_str().unwrap();
        assert!(s.ends_with("[truncated]"));
        assert!(s.len() < MAX_PAYLOAD_SIZE + 100);

        let (unicode, truncated) =
            truncate_payload(serde_json::Value::String("€".repeat(MAX_PAYLOAD_SIZE)));
        assert!(truncated);
        assert!(unicode.as_str().unwrap().ends_with("[truncated]"));
    }

    #[test]
    fn to_hook_json_emits_camel_and_snake_tool_aliases() {
        let base = |payload| HookEventEnvelope {
            hook_event_name: HookEventName::PreToolUse,
            session_id: "sess-1".into(),
            cwd: "/repo".into(),
            workspace_root: "/repo".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: Some("/tmp/transcript.jsonl".into()),
            client_identifier: None,
            prompt_id: None,
            permission_mode: Some("default".into()),
            payload,
        };

        let assert_base_aliases = |v: &serde_json::Value| {
            for (camel, snake) in [
                ("sessionId", "session_id"),
                ("transcriptPath", "transcript_path"),
                ("permissionMode", "permission_mode"),
            ] {
                assert_eq!(v.get(camel), v.get(snake), "{camel} != {snake}");
                assert!(
                    v.get(camel).is_some_and(|x| !x.is_null()),
                    "missing base key pair for {camel}"
                );
            }
            assert_eq!(v.get("cwd").and_then(|x| x.as_str()), Some("/repo"));
        };

        let pre = base(HookPayload::PreToolUse {
            tool_name: "run_terminal_command".into(),
            tool_use_id: "tu-1".into(),
            tool_input: serde_json::json!({ "command": "ls" }),
            tool_input_truncated: false,
            subagent_type: None,
        })
        .to_hook_json();
        assert_base_aliases(&pre);
        for (camel, snake) in [
            ("toolName", "tool_name"),
            ("toolInput", "tool_input"),
            ("toolUseId", "tool_use_id"),
        ] {
            assert_eq!(pre.get(camel), pre.get(snake), "{camel} != {snake}");
            assert!(
                pre.get(camel).is_some_and(|x| !x.is_null()),
                "missing tool key pair for {camel}"
            );
        }
        assert_eq!(
            pre.get("hookEventName").and_then(|x| x.as_str()),
            Some("pre_tool_use")
        );
        assert_eq!(
            pre.get("hook_event_name").and_then(|x| x.as_str()),
            Some("PreToolUse")
        );
        assert_eq!(
            pre.get("tool_name").and_then(|x| x.as_str()),
            Some("run_terminal_command")
        );

        let post = HookEventEnvelope {
            hook_event_name: HookEventName::PostToolUse,
            payload: HookPayload::PostToolUse {
                tool_name: "run_terminal_command".into(),
                tool_use_id: "tu-1".into(),
                tool_input: serde_json::json!({ "command": "ls" }),
                tool_result: serde_json::json!({ "stdout": "a\nb\n" }),
                tool_input_truncated: false,
                tool_result_truncated: false,
                duration_ms: Some(12),
                is_backgrounded: false,
                subagent_type: None,
            },
            ..base(HookPayload::PreToolUse {
                tool_name: String::new(),
                tool_use_id: String::new(),
                tool_input: serde_json::Value::Null,
                tool_input_truncated: false,
                subagent_type: None,
            })
        }
        .to_hook_json();
        assert_base_aliases(&post);
        for (camel, snake) in [
            ("toolName", "tool_name"),
            ("toolInput", "tool_input"),
            ("toolUseId", "tool_use_id"),
            ("toolResult", "tool_response"),
        ] {
            assert_eq!(post.get(camel), post.get(snake), "{camel} != {snake}");
            assert!(
                post.get(camel).is_some_and(|x| !x.is_null()),
                "missing tool key pair for {camel}"
            );
        }
        assert_eq!(post.get("toolResult"), post.get("tool_response"));
        assert_eq!(
            post.get("tool_response"),
            Some(&serde_json::json!({ "stdout": "a\nb\n" }))
        );
        assert_eq!(
            post.get("hookEventName").and_then(|x| x.as_str()),
            Some("post_tool_use")
        );
        assert_eq!(
            post.get("hook_event_name").and_then(|x| x.as_str()),
            Some("PostToolUse")
        );
        assert!(post.get("durationMs").is_some());
    }

    #[test]
    fn post_tool_use_failure_carries_duration_and_interrupt() {
        let value = HookEventEnvelope {
            hook_event_name: HookEventName::PostToolUseFailure,
            session_id: "sess-1".into(),
            cwd: "/repo".into(),
            workspace_root: "/repo".into(),
            timestamp: "2025-01-01T00:00:00Z".into(),
            transcript_path: None,
            client_identifier: None,
            prompt_id: None,
            permission_mode: None,
            payload: HookPayload::PostToolUseFailure {
                tool_name: "run_terminal_command".into(),
                tool_use_id: "tu-1".into(),
                tool_input: serde_json::json!({ "command": "ls" }),
                tool_input_truncated: false,
                error: "boom".into(),
                duration_ms: Some(42),
                is_interrupt: true,
                subagent_type: None,
            },
        }
        .to_hook_json();
        assert_eq!(
            value.get("hookEventName").and_then(|x| x.as_str()),
            Some("post_tool_use_failure")
        );
        assert_eq!(
            value.get("hook_event_name").and_then(|x| x.as_str()),
            Some("PostToolUseFailure")
        );
        assert_eq!(value.get("error").and_then(|x| x.as_str()), Some("boom"));
        assert_eq!(value.get("durationMs").and_then(|x| x.as_u64()), Some(42));
        assert_eq!(value.get("duration_ms").and_then(|x| x.as_u64()), Some(42));
        assert_eq!(
            value.get("isInterrupt").and_then(|x| x.as_bool()),
            Some(true)
        );
        assert_eq!(
            value.get("is_interrupt").and_then(|x| x.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn stop_cancelled_wire_shape() {
        let wire_of = |reason: StopCancelledReason| match reason {
            StopCancelledReason::UserInterrupt => ("user_interrupt", "user"),
            StopCancelledReason::PermissionRejected => ("permission_rejected", "user"),
            StopCancelledReason::PermissionCancelled => ("permission_cancelled", "user"),
            StopCancelledReason::MaxTurns => ("max_turns", "runtime"),
            StopCancelledReason::NoProgress => ("no_progress", "runtime"),
            StopCancelledReason::Unknown => ("unknown", "unknown"),
        };
        for reason in <StopCancelledReason as strum::IntoEnumIterator>::iter() {
            let (wire, cancelled_by) = wire_of(reason);
            let payload = HookPayload::StopCancelled {
                reason,
                cancelled_by: reason.cancelled_by(),
                cancel_trigger: None,
                reason_details: None,
                last_assistant_message: None,
                subagent_type: None,
            };
            assert_eq!(payload.match_value(), Some(wire));
            let value = serde_json::to_value(&payload).unwrap();
            assert_eq!(value.get("reason").and_then(|x| x.as_str()), Some(wire));
            assert_eq!(
                value.get("cancelledBy").and_then(|x| x.as_str()),
                Some(cancelled_by)
            );
            assert!(value.get("cancelTrigger").is_none());
        }
    }
}
