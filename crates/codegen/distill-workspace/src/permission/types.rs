// Modified for Distill by Samuel Fajreldines, 2026.
use agent_client_protocol as acp;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionEvent {
    pub tool_id: String,
    pub tool_name: String,
    pub access_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_detail: Option<String>,
    pub yolo_mode: bool,
    pub auto_approved: bool,
    pub user_prompted: bool,
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
    pub timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_denials_consecutive: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_denials_total: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_depth: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_findings: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_verdict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remember_tool_approvals: Option<bool>,
}
#[derive(Debug, Clone)]
pub struct PermissionResolution {
    pub decision: Decision,
    pub event: Option<PermissionEvent>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ClientType {
    #[default]
    #[serde(rename = "generic", alias = "grok-shell", alias = "grok_shell")]
    Generic,
    #[serde(rename = "grok-tui", alias = "grok_tui")]
    GrokTUI,
    #[serde(rename = "grok_web")]
    GrokWeb,
    #[serde(rename = "nebula")]
    Nebula,
    #[serde(rename = "extension")]
    Extension,
    #[serde(rename = "grok-pager", alias = "grok_pager")]
    GrokPager,
    #[serde(rename = "grok_desktop")]
    Desktop,
}
impl ClientType {
    pub fn user_agent_label(&self) -> &'static str {
        match self {
            Self::Generic => "grok-shell",
            Self::GrokTUI => "grok-tui",
            Self::GrokWeb => "grok-web",
            Self::Nebula => "nebula",
            Self::Extension => "grok-code-extension",
            Self::GrokPager => "grok-pager",
            Self::Desktop => "grok-desktop",
        }
    }
    pub fn from_client_identifier(id: Option<&str>) -> Self {
        match id {
            Some("grok-web") => Self::GrokWeb,
            Some("nebula") => Self::Nebula,
            Some("grok-code-extension") => Self::Extension,
            Some("grok-desktop") => Self::Desktop,
            Some("grok-pager") => Self::GrokPager,
            _ => Self::Generic,
        }
    }
    pub fn feedback_label(&self) -> &'static str {
        match self {
            Self::GrokTUI | Self::GrokPager => "tui",
            Self::GrokWeb => "web",
            Self::Nebula => "nebula",
            Self::Extension => "extension",
            Self::Generic => "agent",
            Self::Desktop => "desktop",
        }
    }
    pub const fn can_present_permission_prompt(self) -> bool {
        !matches!(self, Self::Generic)
    }
}
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum AccessKind {
    Read(Option<String>),
    Grep {
        path: Option<String>,
        glob: Option<String>,
    },
    /// A file edit, named by its path.
    Edit(String),
    Bash(String),
    MCPTool {
        name: String,
        input: serde_json::Value,
    },
    WebFetch(String),
    WebSearch(String),
    AgentMessage {
        subagent_id: String,
    },
    /// A mutating tool that is neither a file edit, a command, nor an MCP call (subagent spawn,
    /// scheduler, workflow, generation, deploy, feedback, browser, anything unclassified), named by
    /// its tool id. No grant scope covers it: every call prompts.
    Tool(String),
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    FollowupMessage(String),
    Reject(String),
    PolicyDeny(String),
    Cancelled,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditPolicy {
    #[default]
    Ask,
    Allow,
    Reject,
}
impl Serialize for EditPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::Ask => "ask",
            Self::Allow => "allow",
            Self::Reject => "reject",
        })
    }
}
impl<'de> Deserialize<'de> for EditPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = EditPolicy;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("one of: ask, allow, reject")
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<EditPolicy, E> {
                match v {
                    "ask" => Ok(EditPolicy::Ask),
                    "allow" => Ok(EditPolicy::Allow),
                    "reject" => Ok(EditPolicy::Reject),
                    other => Err(E::unknown_variant(other, &["ask", "allow", "reject"])),
                }
            }
        }
        deserializer.deserialize_str(V)
    }
}
#[derive(Debug, Clone)]
pub struct RequestPathContext {
    pub real_cwd: std::path::PathBuf,
    pub display_cwd: Option<std::path::PathBuf>,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HookAsk {
    pub hook_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
pub const HOOK_ASK_META_KEY: &str = "hookAsk";
const HOOK_ASK_SEPARATOR: &str = " — ";
impl HookAsk {
    pub fn ask_line(&self) -> String {
        let hook_name = &self.hook_name;
        let reason = self.reason.as_deref().unwrap_or_default();
        let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
        if reason.is_empty() {
            format!("hook '{hook_name}' asks for confirmation")
        } else {
            format!("hook '{hook_name}' asks: {reason}")
        }
    }
    pub fn prompt_header(&self, action: &str) -> String {
        format!("{action}{HOOK_ASK_SEPARATOR}{}", self.ask_line())
    }
    pub fn strip_prompt_header<'a>(&self, title: &'a str) -> &'a str {
        title
            .strip_suffix(self.ask_line().as_str())
            .and_then(|action| action.strip_suffix(HOOK_ASK_SEPARATOR))
            .unwrap_or(title)
    }
}
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub access: AccessKind,
    pub tool_call_update: acp::ToolCallUpdate,
    pub path_context: Option<RequestPathContext>,
    pub session_id: Option<String>,
    pub subagent_type: Option<String>,
    pub subagent_description: Option<String>,
    pub hook_ask: Option<HookAsk>,
}
impl PermissionRequest {
    pub fn new(access: AccessKind, tool_call_update: acp::ToolCallUpdate) -> Self {
        Self {
            access,
            tool_call_update,
            path_context: None,
            session_id: None,
            subagent_type: None,
            subagent_description: None,
            hook_ask: None,
        }
    }
}
#[allow(clippy::large_enum_variant)]
pub enum PermissionCommand {
    Request {
        request: PermissionRequest,
        respond_to: oneshot::Sender<PermissionResolution>,
    },
    SetYoloMode(bool),
    SetAutoMode(bool),
    SetClassifier(Option<std::sync::Arc<dyn super::auto_mode::PermissionClassifier>>),
    SetClassifierTranscript(Vec<super::auto_mode::ClassifierTurn>),
    SetProjectInstructions(Option<String>),
    ResetState,
    Shutdown,
}
/// Classification is an allowlist of read-only tools: a tool is `Read`/`Grep`/`WebSearch` only when it
/// reads state or touches nothing but the session's own bookkeeping. Every other tool prompts as an
/// edit, a command, an MCP call, or a fetch, so a new `ToolInput` variant cannot run unasked.
impl From<&distill_tools::types::ToolInput> for AccessKind {
    fn from(input: &distill_tools::types::ToolInput) -> Self {
        use distill_tools::types::ToolInput;
        match input {
            ToolInput::ReadFile(r) => AccessKind::Read(Some(r.path.clone())),
            ToolInput::ListDir(l) => AccessKind::Read(Some(l.target_directory.clone())),
            ToolInput::CodexReadFile(r) => AccessKind::Read(Some(r.file_path.clone())),
            ToolInput::CodexListDir(l) => AccessKind::Read(Some(l.dir_path.clone())),
            ToolInput::MemoryGet(m) => AccessKind::Read(Some(m.path.clone())),
            ToolInput::Lsp(l) => AccessKind::Read(l.file_path.clone()),
            ToolInput::Grep(g) => AccessKind::Grep {
                path: g.path.clone(),
                glob: g.glob.clone(),
            },
            ToolInput::CodexGrepFiles(g) => AccessKind::Grep {
                path: g.path.clone(),
                glob: g.include.clone(),
            },
            ToolInput::TodoWrite(_)
            | ToolInput::TaskOutput(_)
            | ToolInput::WaitTasks(_)
            | ToolInput::KillTask(_)
            | ToolInput::Skill(_)
            | ToolInput::MemorySearch(_)
            | ToolInput::SearchTool(_)
            | ToolInput::SchedulerList(_)
            | ToolInput::EnterPlanMode(_)
            | ToolInput::ExitPlanMode(_)
            | ToolInput::AskUserQuestion(_)
            | ToolInput::UpdateGoal(_) => AccessKind::Read(None),
            ToolInput::Task(_) => AccessKind::Tool("task".to_owned()),
            ToolInput::SchedulerCreate(_) => AccessKind::Tool("scheduler_create".to_owned()),
            ToolInput::SchedulerDelete(_) => AccessKind::Tool("scheduler_delete".to_owned()),
            ToolInput::Workflow(_) => AccessKind::Tool("workflow".to_owned()),
            ToolInput::ImageGen(_) => AccessKind::Tool("image_gen".to_owned()),
            ToolInput::ImageEdit(_) => AccessKind::Tool("image_edit".to_owned()),
            ToolInput::ImageToVideo(_) => AccessKind::Tool("image_to_video".to_owned()),
            ToolInput::ReferenceToVideo(_) => AccessKind::Tool("reference_to_video".to_owned()),
            ToolInput::SendSubagentMessage(message) => AccessKind::AgentMessage {
                subagent_id: message.subagent_id.clone(),
            },
            ToolInput::SendFeedback(_) => AccessKind::Tool("send_feedback".to_owned()),
            ToolInput::WebSearch(ws) => AccessKind::WebSearch(ws.query.clone()),
            ToolInput::SearchReplace(search_replace) => {
                AccessKind::Edit(search_replace.file_path.to_string())
            }
            ToolInput::ApplyPatch(_) => AccessKind::Tool("apply_patch".to_owned()),
            ToolInput::HashlineEdit(he) => AccessKind::Edit(he.file_path.to_string()),
            ToolInput::Write(w) => AccessKind::Edit(w.file_path.clone()),
            ToolInput::Bash(bash) => AccessKind::Bash(bash.command.to_string()),
            ToolInput::Monitor(m) => AccessKind::Bash(m.command.clone()),
            ToolInput::MCPTool(mcp) => AccessKind::MCPTool {
                name: mcp.tool_name.to_string(),
                input: mcp.tool_input.clone(),
            },
            ToolInput::UseTool(u) => AccessKind::MCPTool {
                name: u.tool_name.clone(),
                input: u.tool_input.clone(),
            },
            ToolInput::WebFetch(wf) => AccessKind::WebFetch(wf.url.clone()),
            ToolInput::Dynamic(value) => access_kind_from_dynamic(value),
            #[allow(unreachable_patterns)]
            _ => AccessKind::Tool("unclassified_tool".to_owned()),
        }
    }
}
fn dynamic_string_field(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    let object = value.as_object()?;
    keys.iter()
        .find_map(|key| object.get(*key).and_then(serde_json::Value::as_str))
        .map(str::to_owned)
}
fn dynamic_has_field(value: &serde_json::Value, keys: &[&str]) -> bool {
    value
        .as_object()
        .is_some_and(|object| keys.iter().any(|key| object.contains_key(*key)))
}
fn access_kind_from_dynamic(value: &serde_json::Value) -> AccessKind {
    if let Some(name) = dynamic_string_field(value, &["name", "tool", "tool_name", "variant"])
        && (name == "send_feedback" || name == "SendFeedback")
    {
        return AccessKind::Tool("send_feedback".to_owned());
    }
    if let Some(path) = dynamic_string_field(value, &["filePath", "file_path", "path"]) {
        let is_mutation = dynamic_has_field(
            value,
            &[
                "oldString",
                "old_string",
                "newString",
                "new_string",
                "content",
                "edits",
                "replaceAll",
                "replace_all",
            ],
        );
        return if is_mutation {
            AccessKind::Edit(path)
        } else {
            AccessKind::Read(Some(path))
        };
    }
    if let Some(command) = dynamic_string_field(value, &["command"]) {
        return AccessKind::Bash(command);
    }
    AccessKind::Read(None)
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PermissionConfig {
    pub rules: Vec<PermissionRule>,
    #[serde(default)]
    pub prompt_policy: PromptPolicy,
    #[serde(default)]
    pub default_mode_configured: bool,
}
impl PermissionConfig {
    pub fn new(rules: Vec<PermissionRule>) -> Self {
        Self {
            rules,
            prompt_policy: PromptPolicy::Ask,
            default_mode_configured: false,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptPolicy {
    #[default]
    Ask,
    Deny,
    Auto,
    Allow,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRule {
    pub action: RuleAction,
    #[serde(default)]
    pub tool: ToolFilter,
    pub pattern: Option<String>,
    #[serde(default)]
    pub pattern_mode: PatternMode,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PatternMode {
    #[default]
    Glob,
    Domain,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    #[default]
    Deny,
    Ask,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ToolFilter {
    #[default]
    Any,
    Bash,
    Edit,
    Read,
    Grep,
    Mcp,
    WebFetch,
    WebSearch,
    #[serde(rename = "agent_message", alias = "agentmessage")]
    AgentMessage,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementSource {
    Unknown,
    Requirements { path: std::path::PathBuf },
    SystemRequirements { path: std::path::PathBuf },
    ManagedSettings { path: std::path::PathBuf },
    ManagedConfig { path: std::path::PathBuf },
    Config { path: std::path::PathBuf },
    Settings { path: std::path::PathBuf },
}
impl std::fmt::Display for RequirementSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("<unknown>"),
            Self::Requirements { path } => write!(f, "{} (requirements)", path.display()),
            Self::SystemRequirements { path } => {
                write!(f, "{} (system requirements)", path.display())
            }
            Self::ManagedSettings { path } => {
                write!(f, "{} (managed-settings)", path.display())
            }
            Self::ManagedConfig { path } => {
                write!(f, "{} (managed config)", path.display())
            }
            Self::Config { path } => write!(f, "{} (config)", path.display()),
            Self::Settings { path } => write!(f, "{} (settings)", path.display()),
        }
    }
}
#[derive(Debug, Clone)]
pub struct Sourced<T> {
    pub value: T,
    pub source: RequirementSource,
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hook_ask_header_keeps_the_action_and_names_the_hook() {
        let with_reason = HookAsk {
            hook_name: "guard".to_owned(),
            reason: Some("confirm this".to_owned()),
        };
        let header = with_reason.prompt_header("Run `deploy`");
        assert_eq!(header, "Run `deploy` — hook 'guard' asks: confirm this");
        assert_eq!(with_reason.strip_prompt_header(&header), "Run `deploy`");
        let bare = HookAsk {
            hook_name: "guard".to_owned(),
            reason: None,
        };
        assert_eq!(
            bare.prompt_header("Run `deploy`"),
            "Run `deploy` — hook 'guard' asks for confirmation"
        );
        let blank = HookAsk {
            hook_name: "guard".to_owned(),
            reason: Some("  \n".to_owned()),
        };
        assert_eq!(blank.ask_line(), bare.ask_line());
        let multiline = HookAsk {
            hook_name: "guard".to_owned(),
            reason: Some("confirm\nthis".to_owned()),
        };
        assert_eq!(multiline.ask_line(), with_reason.ask_line());
    }
    #[test]
    fn agent_message_tool_filter_serde_is_dedicated_and_unknown_is_rejected() {
        let filter: ToolFilter = serde_json::from_str(r#""agent_message""#).unwrap();
        assert_eq!(filter, ToolFilter::AgentMessage);
        assert_eq!(
            serde_json::to_string(&filter).unwrap(),
            r#""agent_message""#
        );
        assert!(serde_json::from_str::<ToolFilter>(r#""future_tool""#).is_err());
    }
    #[test]
    fn permission_event_subagent_fields_default_to_none() {
        let json = r#"{
            "tool_id": "tc1",
            "tool_name": "bash",
            "access_kind": "bash",
            "yolo_mode": false,
            "auto_approved": false,
            "user_prompted": true,
            "decision": "allow",
            "timestamp": "2026-03-24T00:00:00Z"
        }"#;
        let event: PermissionEvent = serde_json::from_str(json).unwrap();
        assert!(event.subagent_session_id.is_none());
        assert!(event.subagent_type.is_none());
        assert!(event.subagent_description.is_none());
        assert!(event.permission_mode.is_none());
        assert!(event.decision_reason.is_none());
        assert!(event.classifier_source.is_none());
        assert!(event.classifier_latency_ms.is_none());
        assert!(event.auto_denials_consecutive.is_none());
        assert!(event.auto_denials_total.is_none());
        assert!(event.wait_ms.is_none());
        assert!(event.queue_depth.is_none());
        assert!(event.security_findings.is_none());
        assert!(event.classifier_verdict.is_none());
    }
    #[test]
    fn permission_event_findings_none_vs_some_empty_are_distinct() {
        let base = r#"{
            "tool_id": "tc1",
            "tool_name": "bash",
            "access_kind": "bash",
            "yolo_mode": false,
            "auto_approved": false,
            "user_prompted": true,
            "decision": "allow",
            "timestamp": "2026-03-24T00:00:00Z",
            "security_findings": [],
            "classifier_verdict": "block"
        }"#;
        let event: PermissionEvent = serde_json::from_str(base).unwrap();
        assert_eq!(event.security_findings.as_deref(), Some([].as_slice()));
        assert_eq!(event.classifier_verdict.as_deref(), Some("block"));
        let with_tokens: PermissionEvent = serde_json::from_str(&base.replace(
            "\"security_findings\": []",
            "\"security_findings\": [\"opaque_shell\"]",
        ))
        .unwrap();
        assert_eq!(
            with_tokens.security_findings.as_deref(),
            Some(["opaque_shell".to_owned()].as_slice())
        );
    }
    #[test]
    fn permission_event_with_subagent_attribution() {
        let event = PermissionEvent {
            tool_id: "tc1".into(),
            tool_name: "bash".into(),
            access_kind: "bash".into(),
            access_detail: None,
            yolo_mode: false,
            auto_approved: false,
            user_prompted: true,
            decision: "allow".into(),
            prompt_outcome: None,
            reject_reason: None,
            timestamp: Utc::now(),
            subagent_session_id: Some("child-1".into()),
            subagent_type: Some("explore".into()),
            subagent_description: Some("Find endpoints".into()),
            permission_mode: Some("ask".into()),
            decision_reason: Some("needs_user".into()),
            classifier_source: Some("llm".into()),
            classifier_latency_ms: Some(42),
            auto_denials_consecutive: Some(2),
            auto_denials_total: Some(5),
            wait_ms: Some(1234),
            queue_depth: Some(3),
            security_findings: Some(vec!["opaque_shell".into()]),
            classifier_verdict: Some("block".into()),
            remember_tool_approvals: Some(true),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(
            json.get("subagent_session_id").and_then(|v| v.as_str()),
            Some("child-1")
        );
        assert_eq!(
            json.get("subagent_type").and_then(|v| v.as_str()),
            Some("explore")
        );
        assert_eq!(
            json.get("subagent_description").and_then(|v| v.as_str()),
            Some("Find endpoints")
        );
        assert_eq!(
            json.get("permission_mode").and_then(|v| v.as_str()),
            Some("ask")
        );
        assert_eq!(
            json.get("decision_reason").and_then(|v| v.as_str()),
            Some("needs_user")
        );
        assert_eq!(
            json.get("classifier_source").and_then(|v| v.as_str()),
            Some("llm")
        );
        assert_eq!(
            json.get("classifier_latency_ms"),
            Some(&serde_json::json!(42))
        );
        assert_eq!(
            json.get("auto_denials_consecutive"),
            Some(&serde_json::json!(2))
        );
        assert_eq!(json.get("auto_denials_total"), Some(&serde_json::json!(5)));
        assert_eq!(json.get("wait_ms"), Some(&serde_json::json!(1234)));
        assert_eq!(json.get("queue_depth"), Some(&serde_json::json!(3)));
        assert_eq!(
            json.get("security_findings")
                .and_then(|v| v.get(0))
                .and_then(|v| v.as_str()),
            Some("opaque_shell")
        );
        assert_eq!(
            json.get("classifier_verdict").and_then(|v| v.as_str()),
            Some("block")
        );
        assert_eq!(
            json.get("remember_tool_approvals"),
            Some(&serde_json::json!(true))
        );
    }
    #[test]
    fn permission_event_skips_none_optional_fields() {
        let event = PermissionEvent {
            tool_id: "tc1".into(),
            tool_name: "bash".into(),
            access_kind: "bash".into(),
            access_detail: None,
            yolo_mode: false,
            auto_approved: true,
            user_prompted: false,
            decision: "allow".into(),
            prompt_outcome: None,
            reject_reason: None,
            timestamp: Utc::now(),
            subagent_session_id: None,
            subagent_type: None,
            subagent_description: None,
            permission_mode: None,
            decision_reason: None,
            classifier_source: None,
            classifier_latency_ms: None,
            auto_denials_consecutive: None,
            auto_denials_total: None,
            wait_ms: None,
            queue_depth: None,
            security_findings: None,
            classifier_verdict: None,
            remember_tool_approvals: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("subagent_session_id"));
        assert!(!json.contains("subagent_type"));
        assert!(!json.contains("permission_mode"));
        assert!(!json.contains("decision_reason"));
        assert!(!json.contains("classifier_source"));
        assert!(!json.contains("classifier_latency_ms"));
        assert!(!json.contains("auto_denials_consecutive"));
        assert!(!json.contains("auto_denials_total"));
        assert!(!json.contains("wait_ms"));
        assert!(!json.contains("queue_depth"));
        assert!(!json.contains("security_findings"));
        assert!(!json.contains("classifier_verdict"));
        assert!(!json.contains("remember_tool_approvals"));
    }
    #[test]
    fn hashline_edit_maps_to_edit_access() {
        use distill_tools::implementations::distill_hashline::edit::types::HashlineEditInput;
        use distill_tools::types::ToolInput;
        let input = ToolInput::HashlineEdit(HashlineEditInput {
            file_path: "src/main.rs".into(),
            edits: vec![],
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Edit(ref p) if p == "src/main.rs"),
            "HashlineEdit should produce AccessKind::Edit with the file path, got {access:?}"
        );
    }
    #[test]
    fn bash_maps_to_bash_access() {
        use distill_tools::implementations::distill::bash::BashToolInput;
        use distill_tools::types::ToolInput;
        let input = ToolInput::Bash(BashToolInput {
            command: "cargo test".into(),
            timeout: None,
            description: "run tests".into(),
            is_background: false,
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Bash(ref cmd) if cmd == "cargo test"),
            "Bash should produce AccessKind::Bash with the command, got {access:?}"
        );
    }
    #[test]
    fn active_agent_message_maps_to_dedicated_access_without_text() {
        use distill_tools::implementations::distill::send_subagent_message::SendSubagentMessageInput;
        use distill_tools::types::ToolInput;
        let text = "private follow-up";
        let access = AccessKind::from(&ToolInput::SendSubagentMessage(SendSubagentMessageInput {
            subagent_id: "sub-1".into(),
            text: text.into(),
            delivery: None,
            queue: false,
        }));
        let AccessKind::AgentMessage { subagent_id } = access else {
            panic!("active agent messages must use dedicated access")
        };
        assert_eq!(subagent_id, "sub-1");
        assert!(!subagent_id.contains(text));
    }
    #[test]
    fn send_feedback_maps_to_tool_access() {
        use distill_tools::types::ToolInput;
        let input: ToolInput = serde_json::from_value(serde_json::json!({
            "variant": "SendFeedback",
            "title": "Draft title",
            "details": "What happened:\n- The tool failed.",
            "type": "bug"
        }))
        .unwrap();
        assert!(matches!(
            AccessKind::from(&input),
            AccessKind::Tool(name) if name == "send_feedback"
        ));
        assert!(matches!(
            AccessKind::from(&ToolInput::Dynamic(serde_json::json!({
                "name": "send_feedback",
                "title": "Draft title",
                "details": "What happened",
            }))),
            AccessKind::Tool(name) if name == "send_feedback"
        ));
    }
    #[test]
    fn use_tool_maps_to_mcp_tool_access() {
        use distill_tools::implementations::use_tool::UseToolInput;
        use distill_tools::types::ToolInput;
        let input = ToolInput::UseTool(UseToolInput {
            tool_name: "linear__save_issue".into(),
            tool_input: serde_json::json!({ "title" : "test" }),
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(
                access,
                AccessKind::MCPTool { ref name, ref input }
                    if name == "linear__save_issue"
                        && input.get("title").and_then(|v| v.as_str()) == Some("test")
            ),
            "UseTool should produce AccessKind::MCPTool carrying the inner tool name and args, got {access:?}"
        );
    }
    #[test]
    fn monitor_maps_to_bash_access() {
        use distill_tools::implementations::distill::monitor::types::MonitorInput;
        use distill_tools::types::ToolInput;
        let input = ToolInput::Monitor(MonitorInput {
            command: "tail -f /var/log/syslog".into(),
            description: "watch syslog".into(),
            timeout_ms: None,
            persistent: false,
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Bash(ref cmd) if cmd == "tail -f /var/log/syslog"),
            "Monitor runs shell and must map to AccessKind::Bash (not Read), got {access:?}"
        );
    }
    #[test]
    fn search_replace_maps_to_edit_access() {
        use distill_tools::implementations::distill::search_replace::SearchReplaceInput;
        use distill_tools::types::ToolInput;
        let input = ToolInput::SearchReplace(SearchReplaceInput {
            file_path: "lib.rs".into(),
            old_string: "old".into(),
            new_string: "new".into(),
            replace_all: false,
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Edit(ref p) if p == "lib.rs"),
            "SearchReplace should produce AccessKind::Edit, got {access:?}"
        );
    }
    #[test]
    fn web_fetch_maps_to_web_fetch_access() {
        use distill_tools::implementations::distill::web_fetch::WebFetchInput;
        use distill_tools::types::ToolInput;
        let input = ToolInput::WebFetch(WebFetchInput {
            url: "https://custom.example.com/api".into(),
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::WebFetch(ref u) if u == "https://custom.example.com/api"),
            "WebFetch should produce AccessKind::WebFetch with the URL, got {access:?}"
        );
    }
    #[test]
    fn web_search_maps_to_web_search_access() {
        use distill_tools::implementations::distill::web_search::WebSearchInput;
        use distill_tools::types::ToolInput;
        let input = ToolInput::WebSearch(WebSearchInput {
            query: "rust lang".into(),
            allowed_domains: None,
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::WebSearch(ref q) if q == "rust lang"),
            "WebSearch should produce AccessKind::WebSearch with the query, got {access:?}"
        );
    }
    /// The patch text names its files; no grant scope can vouch for them, so a patch prompts every time.
    #[test]
    fn apply_patch_maps_to_tool_access() {
        use distill_tools::implementations::codex::apply_patch::ApplyPatchInput;
        use distill_tools::types::ToolInput;
        let input = ToolInput::ApplyPatch(ApplyPatchInput {
            patch: "*** Begin Patch\n*** Update File: /home/user/.grok/mcp.json\n*** End Patch"
                .to_owned(),
        });
        assert!(matches!(
            AccessKind::from(&input),
            AccessKind::Tool(name) if name == "apply_patch"
        ));
    }
    #[test]
    fn write_tool_maps_to_edit_access() {
        use distill_tools::implementations::opencode::write::WriteInput;
        use distill_tools::types::ToolInput;
        let input = ToolInput::Write(WriteInput {
            file_path: "/tmp/secret.txt".into(),
            content: "overwritten".into(),
        });
        let access = AccessKind::from(&input);
        assert!(
            matches!(access, AccessKind::Edit(ref p) if p == "/tmp/secret.txt"),
            "Write should produce AccessKind::Edit with the file path, got {access:?}"
        );
    }
    #[test]
    fn write_scoped_and_dynamic_inputs_map_to_edit_not_read() {
        use distill_tools::implementations::opencode::edit::EditInput;
        use distill_tools::types::ToolInput;
        use distill_tool_types::TaskToolInput;
        let edit = ToolInput::from(EditInput {
            file_path: "/tmp/denied.txt".into(),
            old_string: "ORIGINAL".into(),
            new_string: "BYPASS".into(),
            replace_all: false,
        });
        assert!(matches!(
            &edit,
            ToolInput::SearchReplace(sr) if sr.file_path == "/tmp/denied.txt"
        ));
        assert!(matches!(
            AccessKind::from(&edit),
            AccessKind::Edit(p) if p == "/tmp/denied.txt"
        ));
        assert!(matches!(
            AccessKind::from(&ToolInput::Task(TaskToolInput {
                prompt: "edit config.toml".into(),
                description: "spawn".into(),
                subagent_type: "general-purpose".into(),
                run_in_background: false,
                capability_mode: None,
                isolation: None,
                resume_from: None,
                cwd: None,
                model: None,
                task_id: None,
            })),
            AccessKind::Tool(name) if name == "task"
        ));
        assert!(matches!(
            AccessKind::from(&ToolInput::Dynamic(serde_json::json!({
                "filePath": "/tmp/denied.txt",
                "oldString": "a",
                "newString": "b",
            }))),
            AccessKind::Edit(p) if p == "/tmp/denied.txt"
        ));
        assert!(matches!(
            AccessKind::from(&ToolInput::Dynamic(serde_json::json!({
                "filePath": "src/main.rs"
            }))),
            AccessKind::Read(Some(p)) if p == "src/main.rs"
        ));
        assert!(matches!(
            AccessKind::from(&ToolInput::Dynamic(serde_json::json!({
                "command": "rm -rf /"
            }))),
            AccessKind::Bash(c) if c == "rm -rf /"
        ));
    }
    #[test]
    fn client_type_deserializes_grok_shell_as_generic() {
        assert_eq!(
            serde_json::from_value::<ClientType>("grok-shell".into()).unwrap(),
            ClientType::Generic,
        );
        assert_eq!(
            serde_json::from_value::<ClientType>("grok_shell".into()).unwrap(),
            ClientType::Generic,
        );
        assert_eq!(
            serde_json::from_value::<ClientType>("generic".into()).unwrap(),
            ClientType::Generic,
        );
    }
}
