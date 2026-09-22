// Modified for Distill by Samuel Fajreldines, 2026.
use crate::implementations::distill::send_subagent_message::SendSubagentMessageOutput;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use strip_ansi_escapes::strip_str;
use distill_tool_types::SubagentCompletedOutput;
/// `(added, removed)` line counts for the `edit.lines` telemetry counter.
pub fn line_diff(old: &str, new: &str) -> (i64, i64) {
    let mut added = 0i64;
    let mut removed = 0i64;
    for change in similar::TextDiff::from_lines(old, new).iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => added += 1,
            similar::ChangeTag::Delete => removed += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (added, removed)
}
/// Wrapper for [`ToolOutput::Text`] so it can round-trip through
/// `#[serde(tag = "type")]` (internally-tagged enums require struct/map
/// payloads, not bare primitives).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextOutput {
    pub text: String,
    /// Background-task id for auto-wake suppression when set (see
    /// [`crate::reminders::task_completion::consumed_completion_ids`]);
    /// omitted from prompts and from JSON when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumed_completion_task_id: Option<String>,
}
impl From<String> for TextOutput {
    fn from(text: String) -> Self {
        Self {
            text,
            consumed_completion_task_id: None,
        }
    }
}
impl From<&str> for TextOutput {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            consumed_completion_task_id: None,
        }
    }
}
/// Wrapper for [`ToolOutput::Dynamic`] so it can round-trip through
/// `#[serde(tag = "type")]` (internally-tagged enums require struct/map
/// payloads, not bare primitives).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicOutput {
    pub value: serde_json::Value,
}
impl From<serde_json::Value> for DynamicOutput {
    fn from(value: serde_json::Value) -> Self {
        Self { value }
    }
}
/// Typed saved path for the media tools (`image_gen` / `video_gen` / `image_edit`), so consumers
/// read it directly instead of scraping the prose. A struct (not a bare `PathBuf`) is required:
/// `ToolOutput` is internally tagged and only accepts map payloads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaGenOutput {
    /// Absolute path to the saved media file. Empty for [`Self::uploaded`].
    pub path: PathBuf,
    /// Basename of the saved media file (for example, `8.jpg`).
    #[serde(default)]
    pub filename: String,
    /// Session-relative media directory name (for example, `images` or `videos`).
    #[serde(default)]
    pub session_folder: String,
    /// Set when the media was uploaded to a remote presigned URL (ZDR video
    /// output) and is not available locally; omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploaded_url: Option<String>,
}
impl MediaGenOutput {
    pub fn new(path: PathBuf) -> Self {
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let session_folder = path
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Self {
            path,
            filename,
            session_folder,
            uploaded_url: None,
        }
    }
    /// Media uploaded to a remote presigned URL and not available locally
    /// (ZDR video output). No local path/filename/session folder.
    pub fn uploaded(url: String) -> Self {
        Self {
            path: PathBuf::new(),
            filename: String::new(),
            session_folder: String::new(),
            uploaded_url: Some(url),
        }
    }
    /// Model-facing prose. `action` is the variant's lead-in
    /// ("Image generated" / "Video generated" / "Image edited"); the trailing
    /// guidance stops the model re-reading or narrating the result.
    pub fn prompt_text(&self, action: &str) -> String {
        if let Some(url) = &self.uploaded_url {
            return format!(
                "{action} and uploaded to {url}. The file is not available locally — reference it by this URL. Do not read or re-display it, and do not describe how it appears to the user."
            );
        }
        let path = self.path.to_string_lossy().to_string();
        let message = format!(
            "{action} and saved to {path}. Do not read or re-display it, and do not describe how it appears to the user."
        );
        serde_json::json!({
            "path": path,
            "filename": &self.filename,
            "session_folder": &self.session_folder,
            "message": message,
        })
        .to_string()
    }
}
use crate::implementations::distill::todo::{TodoItem, TodoState};
use crate::implementations::skills::skill::SkillOutput;
use crate::util::truncate::{DEFAULT_SOFT_WRAP_WIDTH, soft_wrap_lines};
/// Result of running a tool through the ToolRunner pipeline. This is the **single return type**
/// from `ToolRunner::run()`. Clean `output` — never mutated by layers; for JSON serialization,
/// protocol translation. `prompt_text` — rendered with system reminders appended; for model prompt.
#[derive(Debug, Serialize, Deserialize)]
pub struct ToolRunResult {
    /// Clean tool output — never mutated by layers.
    /// Consumers use this for: JSON serialization, protocol translation, hunk tracking.
    pub output: ToolOutput,
    /// Prompt-ready text — layers can append system reminders, etc.
    /// Consumers use this for: model prompt (ConversationItem::tool_result).
    pub prompt_text: String,
    /// When a meta-tool dispatches to a different underlying tool (for example
    /// `use_tool` → `linear__save_issue`), this carries the effective tool name.
    /// `None` means the requested tool and executed tool are the same.
    pub effective_tool_name: Option<String>,
}
impl ToolRunResult {
    /// Like [`TypedToolOutput::from_value`], but reattaches `chat_completion_output` from `output`.
    pub fn into_typed_tool_output(
        self,
        tool_id: distill_tool_protocol::ToolId,
    ) -> distill_tool_runtime::TypedToolOutput {
        typed_tool_output_preserving_cco(tool_id, &self, &self.output)
    }
}
/// Like [`TypedToolOutput::from_value`], but reattaches `chat_completion_output` from `source`.
pub(crate) fn typed_tool_output_preserving_cco(
    tool_id: distill_tool_protocol::ToolId,
    payload: &impl Serialize,
    source: &impl distill_tool_runtime::ToolOutput,
) -> distill_tool_runtime::TypedToolOutput {
    let cco = source.chat_completion_output();
    let value = serde_json::to_value(payload).unwrap_or(serde_json::Value::Null);
    distill_tool_runtime::TypedToolOutput::from_value(tool_id, value).with_chat_completion_output(cco)
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ListDirContent {
    /// Formatted directory listing string
    pub content: String,
    /// Root directory path (absolute) for this listing
    pub absolute_root_path: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ListDirOutput {
    Content(ListDirContent),
    /// Target path does not exist
    NotFound(String),
    /// Target path exists but is a file, not a directory
    IsAFile(String),
    /// Target path exists but is not a directory
    NotADirectory(String),
    /// Permission denied accessing the directory
    PermissionDenied(String),
    /// Generic / unclassified error
    Error(String),
}
pub use distill_tool_types::{GrepFileMatch, GrepLineMatch, GrepSearchOutput};
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct FileContent {
    /// content here is the model friendly output which will always be present since even
    /// on failures we want to present the model with some information
    pub content: String,
    /// Concise version of content (arrow separator, no padding) for models
    /// that use the concise output format
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_concise: Option<String>,
    pub absolute_path: PathBuf,
    pub offset: Option<usize>,
    /// The line limit used for this read. `None` means no limit was applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Contains the raw output from the tool invocation without any formatting
    pub raw_output: String,
    /// Total number of lines in the file. Used by system reminders to detect
    /// offset-past-end vs genuinely-empty files.
    #[serde(default)]
    pub total_lines: usize,
    /// Pre-truncation image captures for session harvest. Must survive
    /// ToolDyn hub `to_value`/`from_value`; session drains before PostToolUse
    /// and ACP wire serialize.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(skip)]
    pub extracted_images: Vec<crate::util::base64_images::ExtractedImage>,
}
/// Image content returned when reading an image file. This is a local type so it can derive `schemars::JsonSchema`
/// v0.8, which the `Tool` trait requires for its `Output` associated type. Conversion to the protocol-level image type
/// happens at the protocol boundary in `distill-shell`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImageContent {
    /// Base64-encoded image data
    pub data: String,
    /// MIME type of the image (e.g., "image/png", "image/jpeg")
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}
/// A single rendered PDF page.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PdfPageImage {
    /// Base64-encoded JPEG data
    pub data: String,
    /// MIME type (always "image/jpeg")
    pub mime_type: String,
    /// 1-based page number
    pub page_number: usize,
}
/// Multiple rendered PDF page images.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PdfPageImages {
    /// Rendered page images, one per requested page
    pub pages: Vec<PdfPageImage>,
    /// Total pages in the PDF document
    pub total_pages: usize,
    /// File size in bytes
    pub file_size: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ReadFileOutput {
    FileContent(FileContent),
    /// Target file does not exist
    FileNotFound(String),
    /// Target path is a directory, not a file
    IsADirectory(String),
    /// Permission denied reading the file
    PermissionDenied(String),
    /// File content exceeds maximum token limit
    FileTooLarge(String),
    /// Generic / unclassified read error
    FileReadError(String),
    ImageContent(ImageContent),
    ImageSizeError(String),
    PdfPageImages(PdfPageImages),
}
/// Represents successful edits applied by SearchReplace
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchReplaceEditsApplied {
    pub old_string: String,
    pub new_string: String,
    pub tool_output_for_prompt: String,
    /// Concise version of tool_output_for_prompt (shorter, no snippet) for
    /// models that use the concise output format
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_output_for_prompt_concise: Option<String>,
    pub absolute_path: PathBuf,
    pub edits: SearchReplaceEditContextInformation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// `true` when the match used Unicode confusable normalization
    /// (exact byte match failed, but normalized match succeeded).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unicode_normalized: bool,
}
/// Contains the edit details present as a struct
#[derive(Debug, Default, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchReplaceEditContextInformation {
    pub details: Vec<SearchReplaceEditDetail>,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SearchReplaceEditDetail {
    /// The exact old string that was matched in the file
    pub old_string: String,
    /// 1-based line number where the match begins in the original file
    pub old_line: usize,
    /// The replacement string that was written
    pub new_string: String,
    /// 1-based line number where the replacement begins in the updated file
    pub new_line: usize,
    /// The context before the match
    pub context_before: String,
    /// The context after the match
    pub context_after: String,
    /// Leading text on the first line before the matched `old_string` begins. When the match starts mid-line (e.g., after
    /// indentation), this captures the prefix so the diff renderer can display proper alignment. Empty when the match
    /// starts at the beginning of a line or when unknown.
    #[serde(default)]
    pub line_prefix: String,
}
/// Output of the codex `grep_files` tool — file paths matching a regex.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub enum CodexGrepFilesOutput {
    /// Matching file paths, one per line.
    Matches { content: String, file_count: usize },
    /// No files matched the pattern.
    NoMatches(String),
    /// Error (e.g., path not found, rg failed).
    Error(String),
}
/// Per-file result included in a successful `ApplyPatchOutput`.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ApplyPatchFileResult {
    /// Absolute path to the affected file.
    pub path: PathBuf,
    /// What happened: `"added"`, `"modified"`, `"deleted"`, or `"moved"`.
    pub action: String,
    /// Full file content before the change. `None` for new files (add).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// Full file content after the change. Empty string for deleted files.
    pub new_text: String,
    /// Destination path (only for moves).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_to: Option<PathBuf>,
}
/// Output of the `apply_patch` tool.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ApplyPatchOutput {
    /// Patch applied successfully.
    Success {
        files: Vec<ApplyPatchFileResult>,
        tool_output_for_prompt: String,
    },
    /// Patch text could not be parsed.
    ParseError(String),
    /// Patch parsed but could not be applied to the filesystem.
    ApplicationError(String),
    /// No hunks in the patch.
    EmptyPatch(String),
}
/// Payload for `SearchReplaceOutput::NoMatchesFound`. Separate struct so consumers (reminders,
/// outcome trackers) can extract the file path without needing to know the call-site context.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NoMatchesFoundError {
    /// Human-readable error message shown to the model.
    pub message: String,
    /// Canonical absolute path of the file that was searched.
    pub file_path: std::path::PathBuf,
    /// Full file text from the same read the edit used when reporting no match. In-process only: never serialized on the
    /// wire (avoids leaking fresher or broader file content than the read/edit path already loaded). Used for `StrReplace`
    /// fuzzy hints without a second `read_file`.
    #[serde(default, skip_serializing)]
    #[schemars(skip)]
    pub file_snapshot_at_edit: Option<String>,
}
/// Output type for the SearchReplace tool
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub enum SearchReplaceOutput {
    FileAlreadyExists(String),
    EditsApplied(SearchReplaceEditsApplied),
    MultipleMatchesFound(String),
    /// The `old_string` was not found in the file.
    /// Carries the canonical absolute path so reminders and outcome trackers
    /// can do per-file accounting without needing extra context.
    NoMatchesFound(NoMatchesFoundError),
    InvalidInput(String),
    /// Target file does not exist
    FileNotFound(String),
    /// A path component exceeds the OS filename length limit (ENAMETOOLONG)
    FilenameTooLong(String),
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BashOutput {
    pub output: Vec<u8>,
    /// ANSI-stripped and soft-wrapped output for model prompt.
    /// Pre-baked at construction time so `to_prompt_format` is a simple read.
    #[serde(default)]
    pub output_for_prompt: String,
    pub exit_code: i32,
    pub command: String,
    pub truncated: bool,
    pub signal: Option<String>,
    pub timed_out: bool,
    /// describes the intent of this bash command
    pub description: Option<String>,
    /// the current working directory after the command completes
    pub current_dir: String,
    /// Path to the output file where full output is stored.
    /// Use read_file tool to retrieve full output when truncated.
    pub output_file: String,
    /// Total bytes of output (before truncation).
    pub total_bytes: usize,
    /// Incremental output delta (new bytes since last notification). When present, consumers should append to their
    /// accumulated buffer instead of replacing with `output`. When `Some(vec![])`, consumers should clear their accumulated
    /// buffer (reset signal). When `None`, the consumer should use `output` as the full buffer.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub output_delta: Option<Vec<u8>>,
    /// Set by the distill `run_terminal_cmd` implementation when the command was detected as a bare `echo "<msg>"` (or close variant: echo -n,
    /// echo -e, simple printf for literal output, etc.). Telemetry / statistics on this pattern for the distill backend. Potential doom-loop /
    /// stagnation signals (repeated trivial echoes are a common "no progress" signal). Model hints (see BareEchoHintState in the bash tool).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub was_bare_echo: bool,
}
impl BashOutput {
    /// Compute `output_for_prompt` from raw output string.
    /// Strips ANSI escapes and soft-wraps long lines.
    pub fn make_output_for_prompt(raw: &str) -> String {
        let stripped = strip_str(raw);
        soft_wrap_lines(&stripped, DEFAULT_SOFT_WRAP_WIDTH)
    }
}
/// Output when a background task is started (matches the vendor-compat XML format)
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BackgroundTaskStarted {
    /// Unique task ID (UUID) for querying later
    pub task_id: String,
    /// Type of background task (e.g., "bash")
    pub task_type: String,
    /// Path to the output file on disk
    pub output_file: String,
    /// Current status (always "running" when returned)
    pub status: String,
    /// The command that was started
    pub command: String,
    /// Human-readable summary
    pub summary: String,
    /// Pre-resolved hint text telling the model how to retrieve output.
    /// Built by the tool's run() using resolved tool/param names.
    #[serde(default)]
    pub retrieval_hint: String,
    /// Optional pre-formatted prompt body. When set, `to_prompt_format` uses this string verbatim instead of the default
    /// `<task-id>...</task-id>` XML envelope. Used by namespace-specific adapters that need to emit a different
    /// model-visible shape without disturbing the structured fields above (which other consumers still parse).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_formatted: Option<String>,
    /// PID of the spawned shell process, when available. Surfaced by
    /// adapters in their background-start template; left as `None` when the
    /// underlying backend cannot report a PID (e.g. ACP/remote terminals).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}
pub use distill_tool_types::WebSearchOutput;
#[derive(Debug, Clone)]
pub struct WebFetchSourceArtifact {
    /// Session artifact containing the complete converted response.
    pub path: PathBuf,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchOutputLocation {
    /// Absolute path to the complete rendered output.
    pub file_path: String,
    /// Exact file size in bytes.
    pub size_bytes: usize,
    /// Number of lines in the file.
    pub line_count: usize,
}
/// Successful web fetch result with page content.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct WebFetchContent {
    /// The final URL (may differ from input after redirects).
    pub url: String,
    /// Page content converted to markdown (or raw text for non-HTML).
    pub content: String,
    /// Content type: "markdown" for converted HTML, or the original MIME type.
    pub content_type: String,
    /// HTTP status code.
    pub status_code: u16,
    /// Size of the content in bytes (before truncation).
    pub bytes: usize,
    /// Internal path to the complete converted body when Distill persisted overflow.
    #[serde(skip)]
    #[schemars(skip)]
    pub source_artifact: Option<WebFetchSourceArtifact>,
    /// Structurally selected inline fallback when the complete body is unavailable.
    #[serde(skip)]
    #[schemars(skip)]
    pub inline_fallback: Option<String>,
    /// Vendor-compat file location for large rendered output.
    #[serde(
        rename = "outputLocation",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub output_location: Option<WebFetchOutputLocation>,
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub enum WebFetchOutput {
    /// Successful fetch with content.
    Content(WebFetchContent),
    /// Domain is not in the allowed domains list.
    DomainNotAllowed(String),
    /// Server redirected to a different host.
    CrossHostRedirect {
        original_host: String,
        redirect_url: String,
    },
    /// Pre-formatted error message (returned without the `Tool \`X\` failed:`
    /// wrapper that `ToolError` propagation would add).
    Error {
        url: Option<String>,
        message: String,
    },
}
impl WebFetchOutput {
    pub fn to_prompt_format(&self) -> String {
        match self {
            Self::Content(c) => c.content.clone(),
            Self::DomainNotAllowed(domain) => {
                format!(
                    "Error: domain {} is not in the allowed domains list",
                    domain
                )
            }
            Self::CrossHostRedirect {
                original_host,
                redirect_url,
            } => {
                format!(
                    "Error: cross-host redirect from {} to {}. Make a new web_fetch call with the redirect URL if needed.",
                    original_host, redirect_url
                )
            }
            Self::Error {
                url: Some(url),
                message,
            } => {
                format!("Error fetching URL {url}: {message}")
            }
            Self::Error { url: None, message } => format!("Error: {message}"),
        }
    }
}
use distill_tool_types::KillTaskOutput;
use distill_tool_types::TaskOutputOutput;
/// Output schema for the bash tool. The bash tool can either complete synchronously (`Bash`) or be
/// started in the background (`BackgroundTaskStarted`). This enum exists to provide a precise JSON
/// Schema via the `Tool::Output` associated type.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum BashToolOutput {
    Bash(BashOutput),
    BackgroundTaskStarted(BackgroundTaskStarted),
}
impl distill_tool_runtime::ToolOutput for BashToolOutput {
    fn chat_completion_output(&self) -> Option<distill_tool_runtime::ToolChatCompletionResponse> {
        match self {
            Self::Bash(bash) => distill_tool_runtime::ToolOutput::chat_completion_output(bash),
            Self::BackgroundTaskStarted(_) => None,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchToolOutput {
    pub result_count: usize,
    pub content: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, derive_more::From)]
#[serde(tag = "type")]
pub enum ToolOutput {
    Bash(BashOutput),
    BackgroundTaskStarted(BackgroundTaskStarted),
    GrepSearch(GrepSearchOutput),
    ReadFile(ReadFileOutput),
    ListDir(ListDirOutput),
    SearchReplace(SearchReplaceOutput),
    Todo(TodoWriteOutput),
    WebSearch(WebSearchOutput),
    WebFetch(WebFetchOutput),
    MCP(MCPOutput),
    TaskOutput(TaskOutputOutput),
    KillTask(KillTaskOutput),
    Skill(SkillOutput),
    ApplyPatch(ApplyPatchOutput),
    CodexGrepFiles(CodexGrepFilesOutput),
    SearchTool(SearchToolOutput),
    SubagentCompleted(SubagentCompletedOutput),
    EnterPlanMode(EnterPlanModeOutput),
    ExitPlanMode(ExitPlanModeOutput),
    AskUserQuestion(AskUserQuestionOutput),
    #[serde(alias = "SendAgentMessage")]
    SendSubagentMessage(SendSubagentMessageOutput),
    Monitor(crate::implementations::distill::monitor::types::MonitorOutput),
    SchedulerCreate(crate::implementations::distill::scheduler::create::SchedulerCreateOutput),
    SchedulerDelete(crate::implementations::distill::scheduler::delete::SchedulerDeleteOutput),
    SchedulerList(crate::implementations::distill::scheduler::list::SchedulerListOutput),
    UpdateGoal(crate::implementations::distill::update_goal::UpdateGoalOutput),
    Workflow(crate::implementations::distill::workflow::WorkflowToolOutput),
    /// Dynamic output for runtime-registered tools (MCP, test tools, etc.)
    Dynamic(DynamicOutput),
    /// Generic text output for tools that produce simple formatted text
    /// (e.g., memory_search, memory_get). The string is the pre-formatted
    /// prompt text — no additional rendering is needed.
    Text(TextOutput),
    #[from(skip)]
    ImageGen(MediaGenOutput),
    #[from(skip)]
    ImageToVideo(MediaGenOutput),
    #[from(skip)]
    ReferenceToVideo(MediaGenOutput),
    #[from(skip)]
    ImageEdit(MediaGenOutput),
}
impl ToolOutput {
    /// Whether this output is a logical tool failure, for `tool.execution`'s
    /// `success`/`outcome`. Conservative: only known error variants count, so we
    /// never report a *false failure*.
    pub fn is_error(&self) -> bool {
        match self {
            ToolOutput::MCP(m) => m.is_error,
            ToolOutput::Bash(b) => b.exit_code != 0,
            ToolOutput::SearchReplace(SearchReplaceOutput::EditsApplied(_)) => false,
            ToolOutput::SearchReplace(_) => true,
            ToolOutput::ListDir(ListDirOutput::Content(_)) => false,
            ToolOutput::ListDir(_) => true,
            ToolOutput::ReadFile(
                ReadFileOutput::FileContent(_)
                | ReadFileOutput::ImageContent(_)
                | ReadFileOutput::PdfPageImages(_),
            ) => false,
            ToolOutput::ReadFile(_) => true,
            ToolOutput::TaskOutput(TaskOutputOutput::TaskNotFound(_)) => true,
            ToolOutput::KillTask(KillTaskOutput::TaskNotFound(_)) => true,
            ToolOutput::Skill(s) => !s.success,
            ToolOutput::WebFetch(WebFetchOutput::Content(_)) => false,
            ToolOutput::WebFetch(_) => true,
            ToolOutput::ApplyPatch(ApplyPatchOutput::Success { .. }) => false,
            ToolOutput::ApplyPatch(_) => true,
            ToolOutput::CodexGrepFiles(CodexGrepFilesOutput::Error(_)) => true,
            ToolOutput::SendSubagentMessage(output) => {
                matches!(
                output.disposition(),
                crate::implementations::distill::send_subagent_message::SendSubagentMessageDisposition::Rejected
            )
            }
            ToolOutput::Todo(
                TodoWriteOutput::DuplicateId(_) | TodoWriteOutput::InvalidArgument(_),
            ) => true,
            ToolOutput::GrepSearch(g) => g.exit_code > 1,
            _ => false,
        }
    }
    /// Render tool output for inclusion in the model prompt with specified format.
    pub fn to_prompt_format(&self) -> String {
        match self {
            ToolOutput::ReadFile(read_file_output) => match read_file_output {
                ReadFileOutput::FileContent(file_content) if file_content.content.is_empty() => {
                    if file_content.total_lines == 0 {
                        "File is empty.".to_string()
                    } else if file_content
                        .offset
                        .is_some_and(|offset| offset > file_content.total_lines)
                    {
                        format!(
                            "(no lines returned: the requested window is past the end of the \
                             file; the file has {} lines)",
                            file_content.total_lines
                        )
                    } else {
                        "(no lines returned)".to_string()
                    }
                }
                ReadFileOutput::FileContent(file_content) => file_content.content.clone(),
                ReadFileOutput::ImageContent(image_content) => {
                    format!(
                        "[Image content of type: {} is included inline in this tool result]",
                        image_content.mime_type,
                    )
                }
                ReadFileOutput::FileNotFound(error_msg)
                | ReadFileOutput::IsADirectory(error_msg)
                | ReadFileOutput::PermissionDenied(error_msg)
                | ReadFileOutput::FileTooLarge(error_msg)
                | ReadFileOutput::FileReadError(error_msg)
                | ReadFileOutput::ImageSizeError(error_msg) => error_msg.to_owned(),
                ReadFileOutput::PdfPageImages(pdf) => {
                    let page_list: Vec<String> = pdf
                        .pages
                        .iter()
                        .map(|p| p.page_number.to_string())
                        .collect();
                    format!(
                        "[Read PDF: {} pages rendered (pages {}). Total document: {} pages, {:.1} KB]",
                        pdf.pages.len(),
                        page_list.join(", "),
                        pdf.total_pages,
                        pdf.file_size as f64 / 1024.0,
                    )
                }
            },
            ToolOutput::ListDir(list_dir_output) => match list_dir_output {
                ListDirOutput::Content(content) => content.content.clone(),
                ListDirOutput::NotFound(error_msg)
                | ListDirOutput::IsAFile(error_msg)
                | ListDirOutput::NotADirectory(error_msg)
                | ListDirOutput::PermissionDenied(error_msg)
                | ListDirOutput::Error(error_msg) => error_msg.to_owned(),
            },
            ToolOutput::SearchReplace(search_replace_output) => match search_replace_output {
                SearchReplaceOutput::EditsApplied(edits_applied) => {
                    edits_applied.tool_output_for_prompt.to_owned()
                }
                SearchReplaceOutput::NoMatchesFound(e) => e.message.clone(),
                SearchReplaceOutput::FileAlreadyExists(error_string)
                | SearchReplaceOutput::MultipleMatchesFound(error_string)
                | SearchReplaceOutput::InvalidInput(error_string)
                | SearchReplaceOutput::FileNotFound(error_string)
                | SearchReplaceOutput::FilenameTooLong(error_string) => error_string.to_owned(),
            },
            ToolOutput::Bash(bash_output) => bash_output.output_for_prompt.clone(),
            ToolOutput::GrepSearch(grep_search_output) => {
                String::from_utf8_lossy(&grep_search_output.stdout).into_owned()
            }
            ToolOutput::Todo(todo_output) => match todo_output {
                TodoWriteOutput::TodosUpdated(success) => success.summary_for_prompt.to_owned(),
                TodoWriteOutput::DuplicateId(msg) => msg.to_owned(),
                TodoWriteOutput::InvalidArgument(msg) => msg.to_owned(),
            },
            ToolOutput::WebSearch(web_search_output) => {
                if let Some(ref pre) = web_search_output.pre_formatted {
                    pre.clone()
                } else {
                    format!(
                        "Web search results for: \"{}\"\n\n{}",
                        web_search_output.query, web_search_output.content
                    )
                }
            }
            ToolOutput::WebFetch(o) => o.to_prompt_format(),
            ToolOutput::MCP(mcp_output) => match &mcp_output.output {
                MCPOutputDetails::Error(error) => {
                    format!("Failed to call {}: {}", &mcp_output.tool_name, error)
                }
                MCPOutputDetails::OkayOutput(output) => output.to_owned(),
            },
            ToolOutput::BackgroundTaskStarted(bg) => {
                if let Some(body) = bg.pre_formatted.as_deref() {
                    body.to_string()
                } else {
                    format!(
                        "<task-id>{}</task-id>\n\
                         <task-type>{}</task-type>\n\
                         <output-file>{}</output-file>\n\
                         <status>{}</status>\n\
                         <summary>{}</summary>\n\
                         {}",
                        bg.task_id,
                        bg.task_type,
                        bg.output_file,
                        bg.status,
                        bg.summary,
                        bg.retrieval_hint
                    )
                }
            }
            ToolOutput::TaskOutput(task_output) => match task_output {
                TaskOutputOutput::Result(r) => {
                    let mut lines = vec![
                        format!("=== Task {} ===", r.task_id),
                        format!("Command: {}", r.command),
                        format!("Status: {}", r.status),
                        format!("Duration: {:.2}s", r.duration_secs),
                    ];
                    if let Some(code) = r.exit_code {
                        lines.push(format!("Exit Code: {}", code));
                    }
                    if !r.output_file.is_empty() {
                        lines.push(format!("Output File: {}", r.output_file));
                    }
                    lines.push(String::new());
                    lines.push("=== Output ===".to_string());
                    if r.output.is_empty() {
                        if r.status == "running" {
                            lines.push("(no output yet)".to_string());
                        } else {
                            lines.push("(no output)".to_string());
                        }
                    } else {
                        lines.push(r.output.clone());
                    }
                    if r.truncated {
                        lines.push(r.truncation_hint.clone());
                    }
                    lines.join("\n")
                }
                TaskOutputOutput::TaskNotFound(msg) => msg.to_owned(),
                TaskOutputOutput::MultiResult(mr) => {
                    let mut lines = vec![format!("=== Multi-wait ({}) ===", mr.mode)];
                    for r in &mr.results {
                        lines.push(format!("--- Task {} [{}] ---", r.task_id, r.status));
                        lines.push(format!("Command: {}", r.command));
                        lines.push(format!("Status: {}", r.status));
                        lines.push(format!("Duration: {:.2}s", r.duration_secs));
                        lines.push(format!(
                            "Exit Code: {}",
                            r.exit_code
                                .map_or_else(|| "none".to_owned(), |code| code.to_string())
                        ));
                        lines.push(format!(
                            "Output File: {}",
                            if r.output_file.is_empty() {
                                "(none)"
                            } else {
                                r.output_file.as_str()
                            }
                        ));
                        lines.push(format!("Truncated: {}", r.truncated));
                        if r.truncated || !r.truncation_hint.is_empty() {
                            lines.push(format!("Truncation Hint: {}", r.truncation_hint));
                        }
                        lines.push(format!("Raw Output Bytes: {}", r.raw_output_bytes));
                        if !r.output.is_empty() {
                            lines.push(r.output.clone());
                        }
                    }
                    lines.push(format!("\n{}", mr.summary));
                    lines.join("\n")
                }
            },
            ToolOutput::KillTask(kill_output) => match kill_output {
                KillTaskOutput::Result(r) => format!("{}: {}", r.outcome, r.message),
                KillTaskOutput::TaskNotFound(msg) => msg.to_owned(),
            },
            ToolOutput::Skill(skill_output) => skill_output
                .skill_message
                .clone()
                .unwrap_or_else(|| skill_output.tool_result.clone()),
            ToolOutput::ApplyPatch(apply_patch_output) => match apply_patch_output {
                ApplyPatchOutput::Success {
                    tool_output_for_prompt,
                    ..
                } => tool_output_for_prompt.to_owned(),
                ApplyPatchOutput::ParseError(msg)
                | ApplyPatchOutput::ApplicationError(msg)
                | ApplyPatchOutput::EmptyPatch(msg) => msg.to_owned(),
            },
            ToolOutput::CodexGrepFiles(output) => match output {
                CodexGrepFilesOutput::Matches { content, .. } => content.clone(),
                CodexGrepFilesOutput::NoMatches(msg) | CodexGrepFilesOutput::Error(msg) => {
                    msg.clone()
                }
            },
            ToolOutput::SearchTool(out) => out.content.clone(),
            ToolOutput::SubagentCompleted(sub) => {
                let mut text = sub.output.clone();
                if let Some(ref wt) = sub.worktree_path {
                    text.push_str(&format!("\n\n<worktree_path>{wt}</worktree_path>"));
                }
                text.push_str("\n\n");
                text.push_str(&sub.resume_footer());
                text
            }
            ToolOutput::EnterPlanMode(EnterPlanModeOutput::Entered {
                message,
                plan_file_path,
                tool_hints,
                plan_file_seed,
            }) => {
                let ask = &tool_hints.ask_user;
                let exit = &tool_hints.exit_plan;
                let task_hint = if tool_hints.task.is_empty() {
                    String::new()
                } else {
                    format!(
                        "\n     You can use the {} tool with subagent_type=\"explore\" to \
                         parallelize codebase exploration without filling your context window.",
                        tool_hints.task
                    )
                };
                let plan_status = match plan_file_seed {
                    PlanFileSeedStatus::Empty => {
                        format!(
                            "Write your plan to {plan_file_path}. The file exists and is empty."
                        )
                    }
                    PlanFileSeedStatus::NonEmpty => {
                        format!(
                            "Write your plan to {plan_file_path}. The file exists but is not empty."
                        )
                    }
                    PlanFileSeedStatus::Missing(reason) => {
                        let detail = match reason {
                            PlanFileSeedFailure::NotCreated => "The file has not yet been created.",
                            PlanFileSeedFailure::NotAFile => {
                                "A directory already exists at that path."
                            }
                            PlanFileSeedFailure::Inaccessible => "The file could not be accessed.",
                            PlanFileSeedFailure::Unavailable => {
                                "The plan file location is unavailable."
                            }
                        };
                        format!("Write your plan to {plan_file_path}. {detail}")
                    }
                };
                format!(
                    "{message}\n\n\
                     {plan_status}\n\n\
                     In plan mode, you should:\n\
                     1. Thoroughly explore the codebase to understand existing patterns{task_hint}\n\
                     2. Identify similar features, codebase architecture, and understand trade-offs\n\
                     3. Use {ask} if you need to clarify the approach\n\
                     4. Design a concrete implementation strategy\n\
                     5. Write your plan to the plan file above\n\
                     6. When ready, use {exit} to present your plan to the user."
                )
            }
            ToolOutput::ExitPlanMode(exit) => match exit {
                ExitPlanModeOutput::PlanReady {
                    message,
                    plan_content,
                    plan_file_path,
                } => {
                    format!(
                        "{message}\n\nYour plan has been saved at: {plan_file_path}\n\n\
                         ## Plan:\n{plan_content}"
                    )
                }
                ExitPlanModeOutput::EmptyPlan { message, .. } => message.clone(),
            },
            ToolOutput::AskUserQuestion(
                AskUserQuestionOutput::QuestionsSent { message, .. }
                | AskUserQuestionOutput::UserAnswered { message },
            ) => message.clone(),
            ToolOutput::SendSubagentMessage(output) => output.to_string(),
            ToolOutput::Monitor(o) => {
                if o.persistent {
                    format!(
                        "Monitor started (task {}, persistent -- runs until kill_task or session end).\n\
                         You will be notified on each event. Keep working -- do not poll or sleep.\n\
                         Events may arrive while you are waiting for the user -- an event is not their reply.",
                        o.task_id
                    )
                } else {
                    format!(
                        "Monitor started (task {}, timeout {}ms).\n\
                         You will be notified on each event. Keep working -- do not poll or sleep.\n\
                         Events may arrive while you are waiting for the user -- an event is not their reply.",
                        o.task_id, o.timeout_ms
                    )
                }
            }
            ToolOutput::SchedulerCreate(o) => {
                let verb = if o.updated { "updated" } else { "created" };
                format!(
                    "Scheduled task {} (ID: {}, {}).",
                    verb, o.id, o.human_schedule
                )
            }
            ToolOutput::SchedulerDelete(o) => o.message.clone(),
            ToolOutput::SchedulerList(o) => {
                if o.tasks.is_empty() {
                    "No scheduled tasks.".into()
                } else {
                    serde_json::to_string_pretty(&o.tasks).unwrap_or_default()
                }
            }
            ToolOutput::UpdateGoal(o) => o.summary.clone(),
            ToolOutput::Workflow(o) => o.message.clone(),
            ToolOutput::Dynamic(v) => serde_json::to_string_pretty(&v.value).unwrap_or_default(),
            ToolOutput::Text(text) => text.text.clone(),
            ToolOutput::ImageGen(m) => m.prompt_text("Image generated"),
            ToolOutput::ImageToVideo(m) => m.prompt_text("Video generated"),
            ToolOutput::ReferenceToVideo(m) => m.prompt_text("Video generated"),
            ToolOutput::ImageEdit(m) => m.prompt_text("Image edited"),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoWriteSuccess {
    pub summary_for_prompt: String,
    pub todos: Vec<TodoItem>,
    /// Full state snapshot for consumer persistence/restoration.
    #[schemars(skip)]
    pub state: TodoState,
}
/// Output from the TodoWrite tool. Follows the error-as-output-variant pattern (like
/// `ReadFileOutput`, `SearchReplaceOutput`) so consumers (Python side, ACP layer) can distinguish
/// tool-logic errors from infrastructure errors.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub enum TodoWriteOutput {
    /// Successfully updated todo state.
    TodosUpdated(TodoWriteSuccess),
    /// Duplicate todo ID found in the input.
    DuplicateId(String),
    /// Argument validation failed (model-facing message is returned verbatim).
    /// Used so missing-field errors surface as the terse `Invalid argument: …`
    /// line, instead of the framework's wrapper around a `ToolError`.
    InvalidArgument(String),
}
/// Why the session plan file is not a ready (empty/non-empty) file.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PlanFileSeedFailure {
    /// Did not exist and could not be created.
    #[default]
    NotCreated,
    /// A directory (or other non-regular file) occupies the path.
    NotAFile,
    /// Exists but could not be read (permission / other IO error).
    Inaccessible,
    /// No `FileSystem` resource or no absolute path was available to seed.
    Unavailable,
}
/// Result of probing / seeding the session plan file on `enter_plan_mode`. Defaults to
/// `Missing(NotCreated)` when the field is absent on older payloads (fail-closed in
/// `to_prompt_format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanFileSeedStatus {
    /// Not ready; carries why (see [`PlanFileSeedFailure`]).
    Missing(PlanFileSeedFailure),
    /// Present and empty (fresh seed or pre-existing empty).
    Empty,
    /// Present with prior content (re-entry; not truncated).
    NonEmpty,
}
impl Default for PlanFileSeedStatus {
    fn default() -> Self {
        Self::Missing(PlanFileSeedFailure::NotCreated)
    }
}
/// Output from the `EnterPlanMode` tool. Confirms plan mode entry and reports session plan-file
/// seed status. The tool may create an empty session plan file (never truncating non-empty
/// content); broader read-only enforcement is handled by orchestration.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub enum EnterPlanModeOutput {
    /// Successfully signaled plan mode entry.
    Entered {
        /// Confirmation message for the model, nudging it into
        /// exploration/planning behavior.
        message: String,
        /// Absolute or display path to the plan file so the model knows
        /// where to write its plan immediately.
        plan_file_path: String,
        /// Pre-resolved tool name hints for `to_prompt_format()`. Resolved at runtime via
        /// `TemplateRenderer` so no tool names are hardcoded. Falls back to canonical names when
        /// the renderer is unavailable.
        #[serde(default)]
        tool_hints: EnterPlanModeToolHints,
        /// Probe / seed outcome; defaults to `Missing` when absent.
        #[serde(default)]
        plan_file_seed: PlanFileSeedStatus,
    },
}
/// Pre-resolved tool name hints embedded in `EnterPlanModeOutput`.
///
/// Resolved at runtime so `to_prompt_format()` never hardcodes tool names.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct EnterPlanModeToolHints {
    /// Client-facing name for `ask_user_question` (ToolKind::AskUser).
    #[serde(default = "EnterPlanModeToolHints::default_ask_user")]
    pub ask_user: String,
    /// Client-facing name for `exit_plan_mode` (ToolKind::ExitPlan).
    #[serde(default = "EnterPlanModeToolHints::default_exit_plan")]
    pub exit_plan: String,
    /// Client-facing name for the subagent `task` tool (ToolKind::Task).
    /// Empty when the task tool is not registered.
    #[serde(default)]
    pub task: String,
}
impl Default for EnterPlanModeToolHints {
    fn default() -> Self {
        Self {
            ask_user: "ask_user_question".to_owned(),
            exit_plan: "exit_plan_mode".to_owned(),
            task: String::new(),
        }
    }
}
impl EnterPlanModeToolHints {
    fn default_ask_user() -> String {
        "ask_user_question".to_owned()
    }
    fn default_exit_plan() -> String {
        "exit_plan_mode".to_owned()
    }
}
/// Output from the `AskUserQuestion` tool. This is a thin signal — the tool sends the questions to the client via a notification and returns a
/// confirmation. The actual answers come back from the client as the tool result (handled by the orchestration layer). Because the answers are
/// provided by the client asynchronously (the user interacts with a UI), the tool output here just confirms the questions were dispatched.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub enum AskUserQuestionOutput {
    /// Questions were successfully dispatched to the client for user input.
    /// Used during migration fallback when `UserQuestionSender` is not yet
    /// injected by the shell.
    QuestionsSent {
        /// Confirmation message for the model.
        message: String,
        /// Number of questions sent.
        question_count: usize,
    },
    /// The user has responded (or cancelled). The `message` is the fully-formatted tool result
    /// string produced by the format module. All four user paths (accepted, chat about this, skip
    /// interview, cancel) return this variant with `ToolCall` status `Completed`.
    UserAnswered {
        /// Pre-formatted tool result string for the model.
        message: String,
    },
}
/// Output from the `ExitPlanMode` tool. The tool reads the plan file from disk and surfaces its
/// content. The orchestration layer / client is responsible for presenting the plan to the user for
/// approval and determining the exit outcome.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub enum ExitPlanModeOutput {
    /// Plan file had content — surfaced for approval.
    PlanReady {
        /// Confirmation message for the model.
        message: String,
        /// The plan file content read from disk.
        plan_content: String,
        /// Path to the plan file (for the model to reference later).
        plan_file_path: String,
    },
    /// Plan file was empty or did not exist.
    EmptyPlan {
        /// Message informing the model there was no plan content.
        message: String,
        /// Path where the plan file was expected.
        plan_file_path: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MCPOutputDetails {
    OkayOutput(String),
    Error(String),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MCPOutput {
    tool_name: String,
    server_name: String,
    output: MCPOutputDetails,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reconnect_attempted: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auth_retry_attempted: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_timeout: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_error: bool,
    /// Pre-truncation image captures for session harvest (same contract as
    /// [`FileContent::extracted_images`]). Must survive ToolDyn hub
    /// `to_value`/`from_value`; session drains before PostToolUse and ACP.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extracted_images: Vec<crate::util::base64_images::ExtractedImage>,
}
impl MCPOutput {
    pub fn okay_output(tool_name: String, server_name: String, output: String) -> Self {
        Self {
            tool_name,
            server_name,
            output: MCPOutputDetails::OkayOutput(output),
            reconnect_attempted: false,
            auth_retry_attempted: false,
            is_timeout: false,
            is_error: false,
            extracted_images: Vec::new(),
        }
    }
    pub fn errored(tool_name: String, server_name: String, error: String) -> Self {
        Self {
            tool_name,
            server_name,
            output: MCPOutputDetails::Error(error),
            reconnect_attempted: false,
            auth_retry_attempted: false,
            is_timeout: false,
            is_error: true,
            extracted_images: Vec::new(),
        }
    }
    pub fn output(&self) -> &MCPOutputDetails {
        &self.output
    }
    pub fn output_mut(&mut self) -> &mut MCPOutputDetails {
        &mut self.output
    }
}
impl distill_tool_runtime::ToolOutput for ToolOutput {
    fn chat_completion_output(&self) -> Option<distill_tool_runtime::ToolChatCompletionResponse> {
        match self {
            Self::Bash(bash) => distill_tool_runtime::ToolOutput::chat_completion_output(bash),
            Self::SearchReplace(edit) => distill_tool_runtime::ToolOutput::chat_completion_output(edit),
            _ => None,
        }
    }
}
impl SearchReplaceEditsApplied {
    /// Where the client's diff lands in the file. `None` for creations (no
    /// line to point at) and when `replace_all` touched several matches,
    /// since the card renders one snippet.
    fn edit_file_anchor(&self) -> Option<distill_tool_runtime::EditFileAnchor> {
        if self.old_string.is_empty() {
            return None;
        }
        let [edit] = self.edits.details.as_slice() else {
            return None;
        };
        let start_line = u32::try_from(edit.old_line).ok()?;
        Some(distill_tool_runtime::EditFileAnchor { start_line })
    }
}
impl distill_tool_runtime::ToolOutput for SearchReplaceEditsApplied {
    /// Same frame grok-computer's `FileEditTool` sends: an empty, successful
    /// `code_execution_result` shell is what settles the edit card on every
    /// client, and the anchor rides along when there is one. The reducer
    /// projects the anchor ahead of the shell only where clients accept it.
    fn chat_completion_output(&self) -> Option<distill_tool_runtime::ToolChatCompletionResponse> {
        Some(distill_tool_runtime::ToolChatCompletionResponse {
            result: Some(distill_tool_runtime::ToolChatCompletion {
                sender: "assistant".into(),
                message_tag: Some("raw_function_result".into()),
                code_execution_result: Some(distill_tool_runtime::ToolCodeExecutionResult::default()),
                edit_file_result: self.edit_file_anchor(),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}
impl distill_tool_runtime::ToolOutput for BashOutput {
    fn chat_completion_output(&self) -> Option<distill_tool_runtime::ToolChatCompletionResponse> {
        let mut stdout = String::from_utf8_lossy(&self.output).into_owned();
        let mut extra = serde_json::Map::new();
        if self.truncated {
            let shown = crate::util::truncate::format_bytes(self.output.len() as u64);
            let total = crate::util::truncate::format_bytes(self.total_bytes as u64);
            stdout.push_str(&format!(
                "\n[truncated: showing first/last {shown} of {total} - full output at: {}]",
                self.output_file
            ));
            extra.insert("truncated".into(), serde_json::Value::Bool(true));
            extra.insert(
                "total_bytes".into(),
                serde_json::Value::from(self.total_bytes as u64),
            );
            if !self.output_file.is_empty() {
                extra.insert(
                    "output_file".into(),
                    serde_json::Value::String(self.output_file.clone()),
                );
            }
        }
        Some(distill_tool_runtime::ToolChatCompletionResponse {
            result: Some(distill_tool_runtime::ToolChatCompletion {
                sender: "assistant".into(),
                message_tag: Some("raw_function_result".into()),
                code_execution_result: Some(distill_tool_runtime::ToolCodeExecutionResult {
                    stdout,
                    stderr: String::new(),
                    exit_code: self.exit_code,
                    command_timed_out: self.timed_out,
                }),
                extra,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}
impl distill_tool_runtime::ToolOutput for ReadFileOutput {}
impl distill_tool_runtime::ToolOutput for ListDirOutput {}
impl distill_tool_runtime::ToolOutput for SearchReplaceOutput {
    fn chat_completion_output(&self) -> Option<distill_tool_runtime::ToolChatCompletionResponse> {
        match self {
            Self::EditsApplied(applied) => {
                distill_tool_runtime::ToolOutput::chat_completion_output(applied)
            }
            _ => None,
        }
    }
}
impl distill_tool_runtime::ToolOutput for TodoWriteOutput {}
impl distill_tool_runtime::ToolOutput for WebFetchOutput {}
impl distill_tool_runtime::ToolOutput for SkillOutput {}
impl distill_tool_runtime::ToolOutput for ApplyPatchOutput {}
impl distill_tool_runtime::ToolOutput for CodexGrepFilesOutput {}
impl distill_tool_runtime::ToolOutput for SearchToolOutput {}
impl distill_tool_runtime::ToolOutput for EnterPlanModeOutput {}
impl distill_tool_runtime::ToolOutput for ExitPlanModeOutput {}
impl distill_tool_runtime::ToolOutput for AskUserQuestionOutput {}
impl distill_tool_runtime::ToolOutput for MCPOutput {}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::implementations::distill::todo::{TodoPriority, TodoStatus};
    use serde_json::json;
    use distill_tool_types::KillTaskResult;
    use distill_tool_types::{MultiTaskOutputResult, TaskOutputOutput, TaskOutputResult};
    #[test]
    fn send_subagent_message_error_classification_is_closed() {
        use crate::implementations::distill::send_subagent_message::SendSubagentMessageOutput::*;
        for (output, is_error) in [
            (
                Accepted {
                    message_id: "m-1".into(),
                },
                false,
            ),
            (NotFoundOrNotOwned, true),
            (NotActiveOrFinalizing, true),
            (Saturated { max_in_flight: 8 }, true),
            (
                QuotaExceeded {
                    kind: crate::implementations::distill::task::types::ActiveAgentMessageQuotaKind::AttemptOutbound,
                    limit: 32,
                },
                true,
            ),
            (AdmissionUncertain, false),
            (NotAcceptedBeforeDeadline, true),
            (Unsupported, true),
            (
                Limit {
                    max_bytes: 8,
                    observed_bytes: 9,
                },
                true,
            ),
            (ChannelClosed, true),
        ] {
            assert_eq!(ToolOutput::SendSubagentMessage(output).is_error(), is_error);
        }
    }
    #[test]
    fn legacy_send_agent_message_output_envelope_deserializes() {
        let output: ToolOutput = serde_json::from_value(serde_json::json!({
            "type": "SendAgentMessage",
            "outcome": "channel_closed",
        }))
        .expect("legacy output envelope must remain replayable");
        assert!(matches!(
            output,
            ToolOutput::SendSubagentMessage(SendSubagentMessageOutput::ChannelClosed)
        ));
    }
    /// Serialize a ToolOutput to JSON value
    fn to_json(output: ToolOutput) -> serde_json::Value {
        serde_json::to_value(&output).unwrap()
    }
    #[test]
    fn mcp_extracted_images_survive_hub_json_roundtrip() {
        let mut mcp = MCPOutput::okay_output(
            "browser_screenshot".into(),
            "browser-use".into(),
            crate::util::base64_images::IMAGE_CONTENT_PLACEHOLDER.into(),
        );
        let payload = "A".repeat(50_000);
        mcp.extracted_images = vec![crate::util::base64_images::ExtractedImage {
            data: payload.clone(),
            mime_type: "image/png".into(),
        }];
        let v = serde_json::to_value(&mcp).unwrap();
        assert!(
            v.get("extracted_images").is_some(),
            "hub ToolDyn to_value must keep non-empty extracted_images"
        );
        let back: MCPOutput = serde_json::from_value(v).unwrap();
        assert_eq!(back.extracted_images.len(), 1);
        assert_eq!(
            back.extracted_images.first().map(|i| i.data.as_str()),
            Some(payload.as_str())
        );
        assert_eq!(
            back.extracted_images.first().map(|i| i.mime_type.as_str()),
            Some("image/png")
        );
    }
    #[test]
    fn tool_output_mcp_extracted_images_survive_hub_roundtrip() {
        let mut mcp = MCPOutput::okay_output(
            "t".into(),
            "s".into(),
            crate::util::base64_images::IMAGE_CONTENT_PLACEHOLDER.into(),
        );
        let payload = "C".repeat(12_000);
        mcp.extracted_images = vec![crate::util::base64_images::ExtractedImage {
            data: payload.clone(),
            mime_type: "image/webp".into(),
        }];
        let output = ToolOutput::MCP(mcp);
        let v = serde_json::to_value(&output).unwrap();
        let back: ToolOutput = serde_json::from_value(v).unwrap();
        let ToolOutput::MCP(mcp) = back else {
            panic!("expected MCP");
        };
        assert_eq!(mcp.extracted_images.len(), 1);
        assert_eq!(
            mcp.extracted_images.first().map(|i| i.data.as_str()),
            Some(payload.as_str())
        );
    }
    #[test]
    fn file_content_extracted_images_survive_hub_json_roundtrip() {
        let payload = "B".repeat(40_000);
        let fc = FileContent {
            content: crate::util::base64_images::IMAGE_CONTENT_PLACEHOLDER.into(),
            content_concise: None,
            absolute_path: PathBuf::from("/tmp/x.png"),
            offset: None,
            limit: None,
            raw_output: String::new(),
            total_lines: 1,
            extracted_images: vec![crate::util::base64_images::ExtractedImage {
                data: payload.clone(),
                mime_type: "image/jpeg".into(),
            }],
        };
        let v = serde_json::to_value(&fc).unwrap();
        assert!(v.get("extracted_images").is_some());
        let back: FileContent = serde_json::from_value(v).unwrap();
        assert_eq!(back.extracted_images.len(), 1);
        assert_eq!(
            back.extracted_images.first().map(|i| i.data.as_str()),
            Some(payload.as_str())
        );
    }
    #[test]
    fn empty_extracted_images_omitted_from_json() {
        let mcp = MCPOutput::okay_output("t".into(), "s".into(), "plain".into());
        let v = serde_json::to_value(&mcp).unwrap();
        assert!(v.get("extracted_images").is_none());
    }
    #[test]
    fn tool_output_read_file_extracted_images_survive_hub_roundtrip() {
        let payload = "D".repeat(18_000);
        let fc = FileContent {
            content: crate::util::base64_images::IMAGE_CONTENT_PLACEHOLDER.into(),
            content_concise: None,
            absolute_path: PathBuf::from("/tmp/y.png"),
            offset: None,
            limit: None,
            raw_output: String::new(),
            total_lines: 1,
            extracted_images: vec![crate::util::base64_images::ExtractedImage {
                data: payload.clone(),
                mime_type: "image/png".into(),
            }],
        };
        let output = ToolOutput::ReadFile(ReadFileOutput::FileContent(fc));
        let v = serde_json::to_value(&output).unwrap();
        let back: ToolOutput = serde_json::from_value(v).unwrap();
        let ToolOutput::ReadFile(ReadFileOutput::FileContent(fc)) = back else {
            panic!("expected FileContent");
        };
        assert_eq!(fc.extracted_images.len(), 1);
        assert_eq!(
            fc.extracted_images.first().map(|i| i.data.as_str()),
            Some(payload.as_str())
        );
        assert_eq!(
            fc.extracted_images.first().map(|i| i.mime_type.as_str()),
            Some("image/png")
        );
    }
    fn empty_file_content(offset: Option<usize>, total_lines: usize) -> FileContent {
        FileContent {
            content: String::new(),
            content_concise: None,
            absolute_path: PathBuf::from("/tmp/f.txt"),
            offset,
            limit: None,
            raw_output: String::new(),
            total_lines,
            extracted_images: vec![],
        }
    }
    /// An empty file must render an explicit notice, not a blank result.
    #[test]
    fn read_empty_file_prompt_says_file_is_empty() {
        let output = ToolOutput::ReadFile(ReadFileOutput::FileContent(empty_file_content(None, 0)));
        assert_eq!(output.to_prompt_format(), "File is empty.");
    }
    /// An offset beyond the last line must say past-EOF and report the real
    /// line count.
    #[test]
    fn read_past_eof_prompt_reports_line_count() {
        let output = ToolOutput::ReadFile(ReadFileOutput::FileContent(empty_file_content(
            Some(101),
            100,
        )));
        let prompt = output.to_prompt_format();
        assert!(
            prompt.contains("past the end of the file"),
            "expected past-EOF notice, got: {prompt}"
        );
        assert!(
            prompt.contains("100 lines"),
            "expected real line count, got: {prompt}"
        );
    }
    /// An empty window with an in-range offset (e.g. `limit: 0`) must render
    /// the generic notice, not a bogus past-EOF claim.
    #[test]
    fn read_empty_window_in_range_offset_is_not_past_eof() {
        let output = ToolOutput::ReadFile(ReadFileOutput::FileContent(empty_file_content(
            Some(5),
            100,
        )));
        let prompt = output.to_prompt_format();
        assert_eq!(prompt, "(no lines returned)");
        assert!(
            !prompt.contains("past the end of the file"),
            "in-range empty window must not claim past-EOF: {prompt}"
        );
    }
    /// Non-empty content renders unchanged.
    #[test]
    fn read_non_empty_content_renders_verbatim() {
        let mut fc = empty_file_content(None, 3);
        fc.content = "1→a\nb\nc".to_string();
        let output = ToolOutput::ReadFile(ReadFileOutput::FileContent(fc));
        assert_eq!(output.to_prompt_format(), "1→a\nb\nc");
    }
    #[test]
    fn text_output_to_prompt_format_omits_consumed_completion_task_id() {
        let output = ToolOutput::Text(TextOutput {
            text: "Task completed in 100ms with exit code: 0.".into(),
            consumed_completion_task_id: Some("bg-uuid-42".into()),
        });
        assert_eq!(
            output.to_prompt_format(),
            "Task completed in 100ms with exit code: 0."
        );
    }
    #[test]
    fn text_output_serde_omits_consumed_id_when_none() {
        let text = TextOutput {
            text: "hello".into(),
            consumed_completion_task_id: None,
        };
        let json = serde_json::to_value(&text).unwrap();
        let obj = json.as_object().expect("object");
        assert_eq!(obj.len(), 1);
        assert!(!obj.contains_key("consumed_completion_task_id"));
        let round_trip: TextOutput = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip.text, "hello");
        assert!(round_trip.consumed_completion_task_id.is_none());
    }
    #[test]
    fn text_output_serde_includes_consumed_id_when_some() {
        let text = TextOutput {
            text: "Task completed".into(),
            consumed_completion_task_id: Some("task-abc".into()),
        };
        let json = serde_json::to_value(&text).unwrap();
        assert_eq!(
            json.get("consumed_completion_task_id")
                .and_then(|v| v.as_str()),
            Some("task-abc")
        );
        let round_trip: TextOutput = serde_json::from_value(json).unwrap();
        assert_eq!(
            round_trip.consumed_completion_task_id.as_deref(),
            Some("task-abc")
        );
    }
    #[test]
    fn media_gen_output() {
        let cases = [
            (
                ToolOutput::ImageGen(MediaGenOutput::new("/tmp/images/1.jpg".into())),
                "ImageGen",
                "/tmp/images/1.jpg",
                "1.jpg",
                "images",
                "Image generated and saved to /tmp/images/1.jpg. Do not read or re-display it, and do not describe how it appears to the user.",
            ),
            (
                ToolOutput::ImageToVideo(MediaGenOutput::new("/tmp/videos/2.mp4".into())),
                "ImageToVideo",
                "/tmp/videos/2.mp4",
                "2.mp4",
                "videos",
                "Video generated and saved to /tmp/videos/2.mp4. Do not read or re-display it, and do not describe how it appears to the user.",
            ),
            (
                ToolOutput::ReferenceToVideo(MediaGenOutput::new("/tmp/videos/3.mp4".into())),
                "ReferenceToVideo",
                "/tmp/videos/3.mp4",
                "3.mp4",
                "videos",
                "Video generated and saved to /tmp/videos/3.mp4. Do not read or re-display it, and do not describe how it appears to the user.",
            ),
            (
                ToolOutput::ImageEdit(MediaGenOutput::new("/tmp/images/2.jpg".into())),
                "ImageEdit",
                "/tmp/images/2.jpg",
                "2.jpg",
                "images",
                "Image edited and saved to /tmp/images/2.jpg. Do not read or re-display it, and do not describe how it appears to the user.",
            ),
        ];
        for (output, ty, path, filename, session_folder, message) in cases {
            let prompt_json: serde_json::Value =
                serde_json::from_str(&output.to_prompt_format()).unwrap();
            assert_eq!(prompt_json.get("path").and_then(|v| v.as_str()), Some(path));
            assert_eq!(
                prompt_json.get("filename").and_then(|v| v.as_str()),
                Some(filename)
            );
            assert_eq!(
                prompt_json.get("session_folder").and_then(|v| v.as_str()),
                Some(session_folder)
            );
            assert_eq!(
                prompt_json.get("message").and_then(|v| v.as_str()),
                Some(message)
            );
            let json = to_json(output);
            assert_eq!(json.get("type").and_then(|v| v.as_str()), Some(ty));
            assert_eq!(json.get("path").and_then(|v| v.as_str()), Some(path));
            assert_eq!(
                json.get("filename").and_then(|v| v.as_str()),
                Some(filename)
            );
            assert_eq!(
                json.get("session_folder").and_then(|v| v.as_str()),
                Some(session_folder)
            );
            let (ToolOutput::ImageGen(m)
            | ToolOutput::ImageToVideo(m)
            | ToolOutput::ReferenceToVideo(m)
            | ToolOutput::ImageEdit(m)) = serde_json::from_value(json).unwrap()
            else {
                panic!("unexpected variant");
            };
            assert_eq!(m.path, PathBuf::from(path));
            assert_eq!(m.filename, filename);
            assert_eq!(m.session_folder, session_folder);
        }
    }
    #[test]
    fn media_gen_output_uploaded() {
        let url = "https://files.example.com/team/video-abc.mp4";
        let output = ToolOutput::ImageToVideo(MediaGenOutput::uploaded(url.to_string()));
        let prompt = output.to_prompt_format();
        assert!(prompt.contains(url), "prompt must include the upload URL");
        let json = to_json(output);
        assert_eq!(json.get("uploaded_url").and_then(|v| v.as_str()), Some(url));
        assert!(
            json.get("path").is_some(),
            "path field must be present (empty for uploaded)"
        );
        let ToolOutput::ImageToVideo(m) = serde_json::from_value(json).unwrap() else {
            panic!("unexpected variant");
        };
        assert_eq!(m, MediaGenOutput::uploaded(url.to_string()));
    }
    #[test]
    fn read_file_not_found_json() {
        let json =
            to_json(ReadFileOutput::FileNotFound("Error: /tmp/x does not exist.".into()).into());
        assert_eq!(
            json,
            json!({"type": "ReadFile", "FileNotFound": "Error: /tmp/x does not exist."})
        );
    }
    #[test]
    fn read_file_is_a_directory_json() {
        let json =
            to_json(ReadFileOutput::IsADirectory("Error: /tmp is a directory.".into()).into());
        assert_eq!(
            json,
            json!({"type": "ReadFile", "IsADirectory": "Error: /tmp is a directory."})
        );
    }
    #[test]
    fn read_file_permission_denied_json() {
        let json = to_json(
            ReadFileOutput::PermissionDenied("Permission denied: /etc/shadow".into()).into(),
        );
        assert_eq!(
            json,
            json!({"type": "ReadFile", "PermissionDenied": "Permission denied: /etc/shadow"})
        );
    }
    #[test]
    fn read_file_too_large_json() {
        let json = to_json(
            ReadFileOutput::FileTooLarge(
                "File content (37044 tokens) exceeds maximum allowed tokens (25000 tokens).".into(),
            )
            .into(),
        );
        assert_eq!(
            json,
            json!({"type": "ReadFile", "FileTooLarge": "File content (37044 tokens) exceeds maximum allowed tokens (25000 tokens)."})
        );
    }
    #[test]
    fn read_file_generic_error_json() {
        let json = to_json(ReadFileOutput::FileReadError("Failed to read file".into()).into());
        assert_eq!(
            json,
            json!({"type": "ReadFile", "FileReadError": "Failed to read file"})
        );
    }
    #[test]
    fn read_file_image_size_error_json() {
        let json = to_json(ReadFileOutput::ImageSizeError("Image too large".into()).into());
        assert_eq!(
            json,
            json!({"type": "ReadFile", "ImageSizeError": "Image too large"})
        );
    }
    #[test]
    fn list_dir_not_found_json() {
        let json = to_json(ListDirOutput::NotFound("does not exist".into()).into());
        assert_eq!(
            json,
            json!({"type": "ListDir", "NotFound": "does not exist"})
        );
    }
    #[test]
    fn list_dir_is_a_file_json() {
        let json = to_json(ListDirOutput::IsAFile("is a file".into()).into());
        assert_eq!(json, json!({"type": "ListDir", "IsAFile": "is a file"}));
    }
    #[test]
    fn list_dir_not_a_directory_json() {
        let json = to_json(ListDirOutput::NotADirectory("is not a directory".into()).into());
        assert_eq!(
            json,
            json!({"type": "ListDir", "NotADirectory": "is not a directory"})
        );
    }
    #[test]
    fn list_dir_permission_denied_json() {
        let json = to_json(ListDirOutput::PermissionDenied("Permission denied".into()).into());
        assert_eq!(
            json,
            json!({"type": "ListDir", "PermissionDenied": "Permission denied"})
        );
    }
    #[test]
    fn list_dir_generic_error_json() {
        let json = to_json(ListDirOutput::Error("Some error".into()).into());
        assert_eq!(json, json!({"type": "ListDir", "Error": "Some error"}));
    }
    #[test]
    fn search_replace_file_not_found_json() {
        let json = to_json(SearchReplaceOutput::FileNotFound("not found".into()).into());
        assert_eq!(
            json,
            json!({"type": "SearchReplace", "FileNotFound": "not found"})
        );
    }
    #[test]
    fn search_replace_no_matches_json() {
        let json = to_json(
            SearchReplaceOutput::NoMatchesFound(crate::types::output::NoMatchesFoundError {
                message: "no matches".into(),
                file_path: std::path::PathBuf::from("/project/src/main.c"),
                file_snapshot_at_edit: None,
            })
            .into(),
        );
        assert_eq!(
            json,
            json!({
                "type": "SearchReplace",
                "NoMatchesFound": {
                    "message": "no matches",
                    "file_path": "/project/src/main.c"
                }
            })
        );
    }
    #[test]
    fn search_replace_no_matches_omits_file_snapshot_from_json() {
        let err = crate::types::output::NoMatchesFoundError {
            message: "no matches".into(),
            file_path: std::path::PathBuf::from("/project/secret.txt"),
            file_snapshot_at_edit: Some("SECRET_PAYLOAD".repeat(20)),
        };
        let json = to_json(SearchReplaceOutput::NoMatchesFound(err).into());
        let inner = json
            .get("NoMatchesFound")
            .and_then(|v| v.as_object())
            .expect("NoMatchesFound object");
        assert_eq!(inner.len(), 2);
        assert!(!inner.contains_key("file_snapshot_at_edit"));
        assert!(!json.to_string().contains("SECRET_PAYLOAD"));
    }
    #[test]
    fn search_replace_multiple_matches_json() {
        let json = to_json(SearchReplaceOutput::MultipleMatchesFound("3 matches".into()).into());
        assert_eq!(
            json,
            json!({"type": "SearchReplace", "MultipleMatchesFound": "3 matches"})
        );
    }
    #[test]
    fn search_replace_file_already_exists_json() {
        let json = to_json(SearchReplaceOutput::FileAlreadyExists("exists".into()).into());
        assert_eq!(
            json,
            json!({"type": "SearchReplace", "FileAlreadyExists": "exists"})
        );
    }
    #[test]
    fn search_replace_invalid_input_json() {
        let json = to_json(SearchReplaceOutput::InvalidInput("same strings".into()).into());
        assert_eq!(
            json,
            json!({"type": "SearchReplace", "InvalidInput": "same strings"})
        );
    }
    #[test]
    fn search_replace_filename_too_long_json() {
        let json = to_json(SearchReplaceOutput::FilenameTooLong("name too long".into()).into());
        assert_eq!(
            json,
            json!({"type": "SearchReplace", "FilenameTooLong": "name too long"})
        );
    }
    #[test]
    fn apply_patch_parse_error_json() {
        let json = to_json(ApplyPatchOutput::ParseError("Invalid patch: boom".into()).into());
        assert_eq!(
            json,
            json!({"type": "ApplyPatch", "ParseError": "Invalid patch: boom"})
        );
    }
    #[test]
    fn apply_patch_application_error_json() {
        let json = to_json(
            ApplyPatchOutput::ApplicationError("File /tmp/x.rs does not exist".into()).into(),
        );
        assert_eq!(
            json,
            json!({"type": "ApplyPatch", "ApplicationError": "File /tmp/x.rs does not exist"})
        );
    }
    #[test]
    fn apply_patch_empty_patch_json() {
        let json = to_json(ApplyPatchOutput::EmptyPatch("No files were modified.".into()).into());
        assert_eq!(
            json,
            json!({"type": "ApplyPatch", "EmptyPatch": "No files were modified."})
        );
    }
    /// `Success` must not serialize under any of the keys Python treats as a
    /// failure, otherwise a successful patch would be taxed as a tool error.
    #[test]
    fn apply_patch_success_json_is_not_an_error_shape() {
        let json = to_json(
            ApplyPatchOutput::Success {
                files: vec![ApplyPatchFileResult {
                    path: PathBuf::from("/repo/src/main.rs"),
                    action: "modified".into(),
                    old_text: Some("old".into()),
                    new_text: "new".into(),
                    move_to: None,
                }],
                tool_output_for_prompt: "Updated /repo/src/main.rs".into(),
            }
            .into(),
        );
        assert_eq!(
            json.get("type").and_then(|v| v.as_str()),
            Some("ApplyPatch")
        );
        assert!(json.get("Success").is_some(), "missing Success key: {json}");
        for key in ["ParseError", "ApplicationError", "EmptyPatch"] {
            assert!(
                json.get(key).is_none(),
                "success must not serialize under the error key {key}: {json}"
            );
        }
    }
    #[test]
    fn kill_task_result_json() {
        let json = to_json(
            KillTaskOutput::Result(KillTaskResult {
                task_id: "task-1".into(),
                outcome: "killed".into(),
                message: "Task was terminated successfully".into(),
            })
            .into(),
        );
        assert_eq!(json.get("type").and_then(|v| v.as_str()), Some("KillTask"));
        assert!(json.get("Result").is_some(), "missing Result key: {json}");
        assert_eq!(
            json.pointer("/Result/task_id").and_then(|v| v.as_str()),
            Some("task-1")
        );
        assert_eq!(
            json.pointer("/Result/outcome").and_then(|v| v.as_str()),
            Some("killed")
        );
    }
    #[test]
    fn kill_task_not_found_json() {
        let json = to_json(
            KillTaskOutput::TaskNotFound(
                "Task abc not found. No background tasks exist in this session.".into(),
            )
            .into(),
        );
        assert_eq!(
            json,
            json!({
                "type": "KillTask",
                "TaskNotFound": "Task abc not found. No background tasks exist in this session."
            })
        );
    }
    #[test]
    fn kill_task_not_found_round_trip() {
        let original = KillTaskOutput::TaskNotFound("not found".into());
        let serialized = serde_json::to_value(&original).unwrap();
        let deserialized: KillTaskOutput = serde_json::from_value(serialized).unwrap();
        assert!(
            matches!(deserialized, KillTaskOutput::TaskNotFound(ref msg) if msg == "not found")
        );
    }
    #[test]
    fn task_output_result_json() {
        let json = to_json(
            TaskOutputOutput::Result(TaskOutputResult {
                task_id: "task-1".into(),
                command: "sleep 10".into(),
                status: "running".into(),
                exit_code: None,
                started: "2026-03-09T00:00:00Z".into(),
                ended: None,
                duration_secs: 5.0,
                output: "hello".into(),
                output_file: "/tmp/task-1.log".into(),
                truncated: false,
                truncation_hint: String::new(),
                raw_output_bytes: 5,
            })
            .into(),
        );
        assert_eq!(
            json.get("type").and_then(|v| v.as_str()),
            Some("TaskOutput")
        );
        assert!(json.get("Result").is_some(), "missing Result key: {json}");
        assert_eq!(
            json.pointer("/Result/task_id").and_then(|v| v.as_str()),
            Some("task-1")
        );
        assert_eq!(
            json.pointer("/Result/status").and_then(|v| v.as_str()),
            Some("running")
        );
    }
    /// The single-task detail view is duration-only: absolute `started` /
    /// `ended` instants stay on the wire struct but must not reach the prompt.
    #[test]
    fn task_output_prompt_is_duration_only() {
        let out = ToolOutput::TaskOutput(TaskOutputOutput::Result(TaskOutputResult {
            task_id: "task-1".into(),
            command: "sleep 10".into(),
            status: "completed".into(),
            exit_code: Some(0),
            started: "2026-03-09T00:00:00Z".into(),
            ended: Some("2026-03-09T00:00:05Z".into()),
            duration_secs: 5.0,
            output: "hello".into(),
            output_file: "/tmp/task-1.log".into(),
            truncated: false,
            truncation_hint: String::new(),
            raw_output_bytes: 5,
        }));
        let prompt = out.to_prompt_format();
        assert!(prompt.contains("Duration: 5.00s"), "{prompt}");
        assert!(prompt.contains("Output File: /tmp/task-1.log"), "{prompt}");
        assert!(
            !prompt.contains("Started") && !prompt.contains("Ended"),
            "absolute instants must not be model-visible: {prompt}"
        );
        assert!(
            !prompt.contains("2026-03-09"),
            "no wall-clock date may survive into the prompt: {prompt}"
        );
    }
    #[test]
    fn task_output_prompt_omits_empty_output_file() {
        let out = ToolOutput::TaskOutput(TaskOutputOutput::Result(TaskOutputResult {
            task_id: "task-2".into(),
            command: "true".into(),
            status: "completed".into(),
            exit_code: Some(0),
            started: String::new(),
            ended: None,
            duration_secs: 0.1,
            output: "done".into(),
            output_file: String::new(),
            truncated: false,
            truncation_hint: String::new(),
            raw_output_bytes: 4,
        }));
        let prompt = out.to_prompt_format();
        assert!(!prompt.contains("Output File"), "{prompt}");
    }

    #[test]
    fn multi_task_output_prompt_preserves_every_child_status_and_recovery_field() {
        let child = |task_id: &str, status: &str, output_file: &str| TaskOutputResult {
            task_id: task_id.to_owned(),
            command: format!("cargo test -- {task_id}"),
            status: status.to_owned(),
            exit_code: (status == "completed").then_some(0),
            started: "2026-09-22T00:00:00Z".to_owned(),
            ended: (status == "completed").then(|| "2026-09-22T00:00:01Z".to_owned()),
            duration_secs: 1.5,
            output: format!("body for {task_id}"),
            output_file: output_file.to_owned(),
            truncated: task_id == "done",
            truncation_hint: if task_id == "done" {
                "use read_file on /tmp/done.log".to_owned()
            } else {
                String::new()
            },
            raw_output_bytes: 123,
        };
        let out = ToolOutput::TaskOutput(TaskOutputOutput::MultiResult(
            MultiTaskOutputResult {
                mode: "wait_any".to_owned(),
                results: vec![
                    child("done", "completed", "/tmp/done.log"),
                    child("running", "running", "/tmp/running.log"),
                    child("missing", "not_found", ""),
                ],
                summary: "1/3 tasks completed (wait_any)".to_owned(),
            },
        ));
        let prompt = out.to_prompt_format();
        for field in [
            "=== Multi-wait (wait_any) ===",
            "1/3 tasks completed (wait_any)",
            "Task done",
            "Task running",
            "Task missing",
            "Status: completed",
            "Status: running",
            "Status: not_found",
            "Exit Code: 0",
            "Exit Code: none",
            "Duration: 1.50s",
            "Output File: /tmp/done.log",
            "Output File: /tmp/running.log",
            "Output File: (none)",
            "Truncated: true",
            "Truncation Hint: use read_file on /tmp/done.log",
        ] {
            assert!(prompt.contains(field), "missing {field}: {prompt}");
        }
    }

    fn make_result(status: &str, raw_output_bytes: usize) -> TaskOutputResult {
        TaskOutputResult {
            task_id: "t".into(),
            command: "cmd".into(),
            status: status.into(),
            exit_code: None,
            started: "2026-01-01T00:00:00Z".into(),
            ended: None,
            duration_secs: 1.0,
            output: "x".repeat(raw_output_bytes.min(100)),
            output_file: "/tmp/t.log".into(),
            truncated: raw_output_bytes > 100,
            truncation_hint: String::new(),
            raw_output_bytes,
        }
    }
    /// Identical status + raw_output_bytes → same signature.
    #[test]
    fn progress_signature_same_when_no_progress() {
        let a = make_result("running", 1000);
        let b = make_result("running", 1000);
        assert_eq!(
            a.progress_signature(),
            b.progress_signature(),
            "identical results must produce the same progress signature"
        );
    }
    /// Growing raw_output_bytes must produce a different signature even when the
    /// formatted output length is unchanged (truncated output scenario).
    #[test]
    fn progress_signature_differs_on_raw_output_growth() {
        let stagnant = make_result("running", 500_000);
        let growing = make_result("running", 500_001);
        assert_ne!(
            stagnant.progress_signature(),
            growing.progress_signature(),
            "raw output growth must produce a different progress signature"
        );
    }
    /// Status change must produce a different signature.
    #[test]
    fn progress_signature_differs_on_status_change() {
        let running = make_result("running", 100);
        let completed = make_result("completed", 100);
        assert_ne!(
            running.progress_signature(),
            completed.progress_signature(),
            "status change must produce a different progress signature"
        );
    }
    /// `raw_output_bytes` field is populated in JSON output.
    #[test]
    fn task_output_result_raw_output_bytes_in_json() {
        let json = to_json(
            TaskOutputOutput::Result(TaskOutputResult {
                task_id: "t2".into(),
                command: "cmd".into(),
                status: "running".into(),
                exit_code: None,
                started: "2026-03-15T00:00:00Z".into(),
                ended: None,
                duration_secs: 0.0,
                output: "hello world".into(),
                output_file: "/tmp/t2.log".into(),
                truncated: false,
                truncation_hint: String::new(),
                raw_output_bytes: 11,
            })
            .into(),
        );
        assert_eq!(
            json.pointer("/Result/raw_output_bytes")
                .and_then(|v| v.as_u64()),
            Some(11)
        );
    }
    #[test]
    fn task_output_not_found_json() {
        let json = to_json(
            TaskOutputOutput::TaskNotFound(
                "Task xyz not found. Known task IDs: [task-1, task-2]".into(),
            )
            .into(),
        );
        assert_eq!(
            json,
            json!({
                "type": "TaskOutput",
                "TaskNotFound": "Task xyz not found. Known task IDs: [task-1, task-2]"
            })
        );
    }
    #[test]
    fn task_output_not_found_round_trip() {
        let original = TaskOutputOutput::TaskNotFound("not found".into());
        let serialized = serde_json::to_value(&original).unwrap();
        let deserialized: TaskOutputOutput = serde_json::from_value(serialized).unwrap();
        assert!(
            matches!(deserialized, TaskOutputOutput::TaskNotFound(ref msg) if msg == "not found")
        );
    }
    #[test]
    fn todo_write_success_json() {
        let json = to_json(
            TodoWriteOutput::TodosUpdated(TodoWriteSuccess {
                summary_for_prompt: "- [pending] 1: Task A\n".into(),
                todos: vec![TodoItem {
                    content: "Task A".into(),
                    priority: TodoPriority::Medium,
                    status: TodoStatus::Pending,
                    meta: None,
                }],
                state: TodoState::default(),
            })
            .into(),
        );
        assert_eq!(json.get("type").and_then(|v| v.as_str()), Some("Todo"));
        assert!(
            json.get("TodosUpdated").is_some(),
            "missing TodosUpdated key: {json}"
        );
        assert_eq!(
            json.pointer("/TodosUpdated/summary_for_prompt")
                .and_then(|v| v.as_str()),
            Some("- [pending] 1: Task A\n")
        );
        assert_eq!(
            json.pointer("/TodosUpdated/todos/0/content")
                .and_then(|v| v.as_str()),
            Some("Task A")
        );
        assert_eq!(
            json.pointer("/TodosUpdated/todos/0/status")
                .and_then(|v| v.as_str()),
            Some("pending")
        );
    }
    #[test]
    fn todo_write_duplicate_id_json() {
        let json = to_json(
            TodoWriteOutput::DuplicateId(
                "Duplicate todo ID in request: \"dup\". Each todo item must have a unique ID."
                    .into(),
            )
            .into(),
        );
        assert_eq!(
            json,
            json!({
                "type": "Todo",
                "DuplicateId": "Duplicate todo ID in request: \"dup\". Each todo item must have a unique ID."
            })
        );
    }
    #[test]
    fn todo_write_duplicate_id_round_trip() {
        let original = TodoWriteOutput::DuplicateId("dup id".into());
        let serialized = serde_json::to_value(&original).unwrap();
        let deserialized: TodoWriteOutput = serde_json::from_value(serialized).unwrap();
        assert!(matches!(deserialized, TodoWriteOutput::DuplicateId(ref msg) if msg == "dup id"));
    }
    #[test]
    fn todo_write_success_round_trip() {
        let original = TodoWriteOutput::TodosUpdated(TodoWriteSuccess {
            summary_for_prompt: "summary".into(),
            todos: vec![TodoItem {
                content: "task".into(),
                priority: TodoPriority::High,
                status: TodoStatus::InProgress,
                meta: None,
            }],
            state: TodoState::default(),
        });
        let serialized = serde_json::to_value(&original).unwrap();
        let deserialized: TodoWriteOutput = serde_json::from_value(serialized).unwrap();
        match deserialized {
            TodoWriteOutput::TodosUpdated(s) => {
                assert_eq!(s.summary_for_prompt, "summary");
                assert_eq!(s.todos.len(), 1);
                let Some(todo) = s.todos.first() else {
                    panic!("expected a todo: {:?}", s.todos);
                };
                assert_eq!(todo.content, "task");
                assert_eq!(todo.status, TodoStatus::InProgress);
                assert_eq!(todo.priority, TodoPriority::High);
            }
            other => panic!("expected TodosUpdated, got {other:?}"),
        }
    }
    #[test]
    fn subagent_completed_prompt_format_includes_resume_footer() {
        let output = ToolOutput::SubagentCompleted(SubagentCompletedOutput {
            output: "I found the auth middleware.".into(),
            subagent_id: "019e0000-0000-7000-8000-0000000000bb".into(),
            subagent_type: "explore".into(),
            tool_calls: 5,
            turns: 2,
            duration_ms: 3000,
            worktree_path: None,
            persona: None,
            resume_from_hint: "019e0000-0000-7000-8000-0000000000bb".into(),
            persona_hint: None,
        });
        let rendered = output.to_prompt_format();
        assert!(
            rendered.contains("I found the auth middleware."),
            "original output preserved"
        );
        assert!(
            rendered.contains("subagent_id: 019e0000-0000-7000-8000-0000000000bb"),
            "subagent_id visible in rendered text"
        );
        assert!(
            rendered.contains("resume_from=\"019e0000-0000-7000-8000-0000000000bb\""),
            "resume_from hint with correct ID"
        );
        assert!(
            rendered.contains("subagent_type: explore"),
            "subagent_type visible"
        );
        assert!(
            rendered.contains("<subagent_result>"),
            "wrapped in subagent_result tag"
        );
        assert!(
            !rendered.contains("persona"),
            "no persona hint when persona is None"
        );
    }
    #[test]
    fn subagent_completed_prompt_format_includes_persona_hint() {
        let output = ToolOutput::SubagentCompleted(SubagentCompletedOutput {
            output: "Done implementing.".into(),
            subagent_id: "abc-123".into(),
            subagent_type: "general-purpose".into(),
            tool_calls: 10,
            turns: 3,
            duration_ms: 5000,
            worktree_path: None,
            persona: Some("implementer".into()),
            resume_from_hint: "abc-123".into(),
            persona_hint: Some("implementer".into()),
        });
        let rendered = output.to_prompt_format();
        assert!(
            rendered.contains("resume_from=\"abc-123\""),
            "resume hint present"
        );
        assert!(
            rendered.contains("persona=\"implementer\""),
            "persona hint present"
        );
    }
    #[test]
    fn subagent_completed_prompt_format_with_worktree() {
        let output = ToolOutput::SubagentCompleted(SubagentCompletedOutput {
            output: "Changes committed.".into(),
            subagent_id: "wt-agent".into(),
            subagent_type: "general-purpose".into(),
            tool_calls: 3,
            turns: 1,
            duration_ms: 2000,
            worktree_path: Some("/tmp/grok-worktree/wt-agent".into()),
            persona: None,
            resume_from_hint: "wt-agent".into(),
            persona_hint: None,
        });
        let rendered = output.to_prompt_format();
        assert!(
            rendered.contains("<worktree_path>/tmp/grok-worktree/wt-agent</worktree_path>"),
            "worktree_path preserved"
        );
        assert!(
            rendered.contains("resume_from=\"wt-agent\""),
            "resume footer still present with worktree"
        );
    }
    #[test]
    fn subagent_completed_structured_hints_serialize() {
        let output = SubagentCompletedOutput {
            output: "done".into(),
            subagent_id: "sub-abc-123".into(),
            subagent_type: "general-purpose".into(),
            tool_calls: 5,
            turns: 2,
            duration_ms: 3000,
            worktree_path: None,
            persona: Some("implementer".into()),
            resume_from_hint: "sub-abc-123".into(),
            persona_hint: Some("implementer".into()),
        };
        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(
            json.get("resume_from_hint").and_then(|v| v.as_str()),
            Some("sub-abc-123")
        );
        assert_eq!(
            json.get("persona_hint").and_then(|v| v.as_str()),
            Some("implementer")
        );
        assert_eq!(json.get("subagent_id"), json.get("resume_from_hint"));
    }
    #[test]
    fn enter_plan_mode_tool_hints_default() {
        let hints = EnterPlanModeToolHints::default();
        assert_eq!(hints.ask_user, "ask_user_question");
        assert_eq!(hints.exit_plan, "exit_plan_mode");
        assert!(hints.task.is_empty());
    }
    #[test]
    fn enter_plan_mode_tool_hints_serde_round_trip() {
        let hints = EnterPlanModeToolHints {
            ask_user: "AskUser".into(),
            exit_plan: "FinishPlan".into(),
            task: "task".into(),
        };
        let json = serde_json::to_value(&hints).unwrap();
        assert_eq!(
            json.get("ask_user").and_then(|v| v.as_str()),
            Some("AskUser")
        );
        assert_eq!(
            json.get("exit_plan").and_then(|v| v.as_str()),
            Some("FinishPlan")
        );
        assert_eq!(json.get("task").and_then(|v| v.as_str()), Some("task"));
        let deserialized: EnterPlanModeToolHints = serde_json::from_value(json).unwrap();
        assert_eq!(deserialized.ask_user, "AskUser");
        assert_eq!(deserialized.exit_plan, "FinishPlan");
        assert_eq!(deserialized.task, "task");
    }
    #[test]
    fn enter_plan_mode_tool_hints_defaults_on_missing_fields() {
        let json = json!({});
        let hints: EnterPlanModeToolHints = serde_json::from_value(json).unwrap();
        assert_eq!(hints.ask_user, "ask_user_question");
        assert_eq!(hints.exit_plan, "exit_plan_mode");
        assert!(hints.task.is_empty());
    }
    #[test]
    fn enter_plan_mode_prompt_format_with_default_hints() {
        let output = ToolOutput::EnterPlanMode(EnterPlanModeOutput::Entered {
            message: "entered-msg-token".into(),
            plan_file_path: "/tmp/plan.md".into(),
            tool_hints: EnterPlanModeToolHints::default(),
            plan_file_seed: PlanFileSeedStatus::Empty,
        });
        let prompt = output.to_prompt_format();
        assert!(prompt.contains("entered-msg-token"));
        assert!(prompt.contains("/tmp/plan.md"));
        assert!(prompt.contains("ask_user_question"));
        assert!(prompt.contains("exit_plan_mode"));
        assert!(
            !prompt.contains("subagent_type"),
            "should not contain subagent guidance without task tool"
        );
    }
    #[test]
    fn enter_plan_mode_prompt_format_with_task_tool() {
        let output = ToolOutput::EnterPlanMode(EnterPlanModeOutput::Entered {
            message: "Entered plan mode.".into(),
            plan_file_path: "/tmp/plan.md".into(),
            tool_hints: EnterPlanModeToolHints {
                ask_user: "ask_user_question".into(),
                exit_plan: "exit_plan_mode".into(),
                task: "delegate-xyz".into(),
            },
            plan_file_seed: PlanFileSeedStatus::Empty,
        });
        let prompt = output.to_prompt_format();
        assert!(prompt.contains("delegate-xyz"));
        assert!(prompt.contains("subagent_type"));
    }
    #[test]
    fn enter_plan_mode_prompt_format_with_custom_tool_names() {
        let output = ToolOutput::EnterPlanMode(EnterPlanModeOutput::Entered {
            message: "Entered plan mode.".into(),
            plan_file_path: "/session/plan.md".into(),
            tool_hints: EnterPlanModeToolHints {
                ask_user: "AskUser".into(),
                exit_plan: "FinishPlan".into(),
                task: String::new(),
            },
            plan_file_seed: PlanFileSeedStatus::Empty,
        });
        let prompt = output.to_prompt_format();
        assert!(prompt.contains("AskUser"));
        assert!(prompt.contains("FinishPlan"));
        assert!(!prompt.contains("ask_user_question"));
        assert!(!prompt.contains("exit_plan_mode"));
    }
    #[test]
    fn enter_plan_mode_output_serde_with_tool_hints() {
        let output = EnterPlanModeOutput::Entered {
            message: "Entered plan mode.".into(),
            plan_file_path: "/tmp/plan.md".into(),
            tool_hints: EnterPlanModeToolHints {
                ask_user: "AskUser".into(),
                exit_plan: "FinishPlan".into(),
                task: "delegate".into(),
            },
            plan_file_seed: PlanFileSeedStatus::Empty,
        };
        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(
            json.pointer("/Entered/tool_hints/ask_user")
                .and_then(|v| v.as_str()),
            Some("AskUser")
        );
        assert_eq!(
            json.pointer("/Entered/tool_hints/exit_plan")
                .and_then(|v| v.as_str()),
            Some("FinishPlan")
        );
        assert_eq!(
            json.pointer("/Entered/tool_hints/task")
                .and_then(|v| v.as_str()),
            Some("delegate")
        );
        assert_eq!(
            json.pointer("/Entered/plan_file_seed")
                .and_then(|v| v.as_str()),
            Some("empty")
        );
        let deserialized: EnterPlanModeOutput = serde_json::from_value(json).unwrap();
        match deserialized {
            EnterPlanModeOutput::Entered {
                tool_hints,
                plan_file_seed,
                ..
            } => {
                assert_eq!(tool_hints.ask_user, "AskUser");
                assert_eq!(tool_hints.exit_plan, "FinishPlan");
                assert_eq!(tool_hints.task, "delegate");
                assert_eq!(plan_file_seed, PlanFileSeedStatus::Empty);
            }
        }
    }
    #[test]
    fn enter_plan_mode_output_serde_defaults_tool_hints_when_absent() {
        let json = json!({
            "Entered": {
                "message": "Entered plan mode.",
                "plan_file_path": "/tmp/plan.md"
            }
        });
        let deserialized: EnterPlanModeOutput = serde_json::from_value(json).unwrap();
        match deserialized {
            EnterPlanModeOutput::Entered {
                tool_hints,
                plan_file_seed,
                ..
            } => {
                assert_eq!(tool_hints.ask_user, "ask_user_question");
                assert_eq!(tool_hints.exit_plan, "exit_plan_mode");
                assert!(tool_hints.task.is_empty());
                assert_eq!(
                    plan_file_seed,
                    PlanFileSeedStatus::Missing(PlanFileSeedFailure::NotCreated)
                );
            }
        }
    }
    #[test]
    fn enter_plan_mode_prompt_format_nonempty_seed() {
        let output = ToolOutput::EnterPlanMode(EnterPlanModeOutput::Entered {
            message: "Entered plan mode.".into(),
            plan_file_path: "/tmp/plan.md".into(),
            tool_hints: EnterPlanModeToolHints::default(),
            plan_file_seed: PlanFileSeedStatus::NonEmpty,
        });
        let empty = ToolOutput::EnterPlanMode(EnterPlanModeOutput::Entered {
            message: "Entered plan mode.".into(),
            plan_file_path: "/tmp/plan.md".into(),
            tool_hints: EnterPlanModeToolHints::default(),
            plan_file_seed: PlanFileSeedStatus::Empty,
        })
        .to_prompt_format();
        let prompt = output.to_prompt_format();
        assert!(prompt.contains("/tmp/plan.md"));
        assert_ne!(prompt, empty, "non-empty seed must change compiled status");
    }
    #[test]
    fn enter_plan_mode_prompt_format_missing_seed() {
        let output = ToolOutput::EnterPlanMode(EnterPlanModeOutput::Entered {
            message: "Entered plan mode.".into(),
            plan_file_path: "/tmp/plan.md".into(),
            tool_hints: EnterPlanModeToolHints::default(),
            plan_file_seed: PlanFileSeedStatus::Missing(PlanFileSeedFailure::NotCreated),
        });
        let empty = ToolOutput::EnterPlanMode(EnterPlanModeOutput::Entered {
            message: "Entered plan mode.".into(),
            plan_file_path: "/tmp/plan.md".into(),
            tool_hints: EnterPlanModeToolHints::default(),
            plan_file_seed: PlanFileSeedStatus::Empty,
        })
        .to_prompt_format();
        let prompt = output.to_prompt_format();
        assert!(prompt.contains("/tmp/plan.md"));
        assert_ne!(prompt, empty, "missing seed must change compiled status");
    }
    #[test]
    fn enter_plan_mode_absent_seed_field_prompt_is_missing() {
        let json = json!({
            "Entered": {
                "message": "Entered plan mode.",
                "plan_file_path": "/tmp/plan.md"
            }
        });
        let deserialized: EnterPlanModeOutput = serde_json::from_value(json).unwrap();
        let prompt = ToolOutput::EnterPlanMode(deserialized).to_prompt_format();
        let empty = ToolOutput::EnterPlanMode(EnterPlanModeOutput::Entered {
            message: "Entered plan mode.".into(),
            plan_file_path: "/tmp/plan.md".into(),
            tool_hints: EnterPlanModeToolHints::default(),
            plan_file_seed: PlanFileSeedStatus::Empty,
        })
        .to_prompt_format();
        assert!(prompt.contains("/tmp/plan.md"));
        assert_ne!(
            prompt, empty,
            "absent seed field must compile as missing, not empty"
        );
    }
    #[test]
    fn enter_plan_mode_missing_reason_suffixes() {
        let cases = [
            PlanFileSeedFailure::NotCreated,
            PlanFileSeedFailure::NotAFile,
            PlanFileSeedFailure::Inaccessible,
            PlanFileSeedFailure::Unavailable,
        ];
        let mut compiled = Vec::new();
        for reason in cases {
            let prompt = ToolOutput::EnterPlanMode(EnterPlanModeOutput::Entered {
                message: "entered-msg-token".into(),
                plan_file_path: "/tmp/plan.md".into(),
                tool_hints: EnterPlanModeToolHints::default(),
                plan_file_seed: PlanFileSeedStatus::Missing(reason),
            })
            .to_prompt_format();
            assert!(prompt.contains("entered-msg-token"), "reason {reason:?}");
            assert!(
                prompt.contains("/tmp/plan.md"),
                "reason {reason:?}: {prompt}"
            );
            compiled.push(prompt);
        }
        compiled.sort();
        compiled.dedup();
        assert_eq!(
            compiled.len(),
            cases.len(),
            "each missing-seed reason must compile distinctly"
        );
    }
    #[test]
    fn plan_file_seed_missing_serde_shape() {
        let output = EnterPlanModeOutput::Entered {
            message: "m".into(),
            plan_file_path: "/tmp/plan.md".into(),
            tool_hints: EnterPlanModeToolHints::default(),
            plan_file_seed: PlanFileSeedStatus::Missing(PlanFileSeedFailure::NotAFile),
        };
        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(
            json.pointer("/Entered/plan_file_seed"),
            Some(&json!({ "missing": "not_a_file" }))
        );
        let back: EnterPlanModeOutput = serde_json::from_value(json).unwrap();
        let EnterPlanModeOutput::Entered { plan_file_seed, .. } = back;
        assert_eq!(
            plan_file_seed,
            PlanFileSeedStatus::Missing(PlanFileSeedFailure::NotAFile)
        );
    }
    #[test]
    fn subagent_completed_hints_absent_when_no_persona() {
        let output = SubagentCompletedOutput {
            output: "done".into(),
            subagent_id: "sub-xyz".into(),
            subagent_type: "explore".into(),
            tool_calls: 1,
            turns: 1,
            duration_ms: 500,
            worktree_path: None,
            persona: None,
            resume_from_hint: "sub-xyz".into(),
            persona_hint: None,
        };
        let json = serde_json::to_value(&output).unwrap();
        assert_eq!(
            json.get("resume_from_hint").and_then(|v| v.as_str()),
            Some("sub-xyz")
        );
        assert!(
            json.get("persona_hint").is_none(),
            "persona_hint should be absent when None"
        );
    }
    fn make_pdf_page_images(
        page_numbers: &[usize],
        total_pages: usize,
        file_size: usize,
    ) -> PdfPageImages {
        PdfPageImages {
            pages: page_numbers
                .iter()
                .map(|&n| PdfPageImage {
                    data: format!("base64data_page{n}"),
                    mime_type: "image/jpeg".to_string(),
                    page_number: n,
                })
                .collect(),
            total_pages,
            file_size,
        }
    }
    #[test]
    fn pdf_page_images_to_prompt_format() {
        let pdf = make_pdf_page_images(&[1, 2, 3], 25, 102400);
        let output = ToolOutput::ReadFile(ReadFileOutput::PdfPageImages(pdf));
        let text = output.to_prompt_format();
        assert!(text.contains("3 pages rendered"), "got: {text}");
        assert!(text.contains("pages 1, 2, 3"), "got: {text}");
        assert!(text.contains("25 pages"), "got: {text}");
        assert!(text.contains("100.0 KB"), "got: {text}");
    }
    #[test]
    fn pdf_page_images_to_prompt_format_single_page() {
        let pdf = make_pdf_page_images(&[5], 10, 51200);
        let output = ToolOutput::ReadFile(ReadFileOutput::PdfPageImages(pdf));
        let text = output.to_prompt_format();
        assert!(text.contains("1 pages rendered"), "got: {text}");
        assert!(text.contains("pages 5"), "got: {text}");
        assert!(text.contains("10 pages"), "got: {text}");
        assert!(text.contains("50.0 KB"), "got: {text}");
    }
    #[test]
    fn pdf_page_images_json_round_trip() {
        let pdf = make_pdf_page_images(&[1, 3], 20, 8192);
        let output = ToolOutput::ReadFile(ReadFileOutput::PdfPageImages(pdf));
        let json = to_json(output);
        assert_eq!(json.get("type").and_then(|v| v.as_str()), Some("ReadFile"));
        assert!(
            json.get("PdfPageImages").is_some(),
            "missing PdfPageImages key: {json}"
        );
        let Some(inner) = json.get("PdfPageImages") else {
            panic!("missing PdfPageImages key: {json}");
        };
        assert_eq!(inner.get("total_pages").and_then(|v| v.as_u64()), Some(20));
        assert_eq!(inner.get("file_size").and_then(|v| v.as_u64()), Some(8192));
        assert_eq!(
            inner
                .get("pages")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(2)
        );
        assert_eq!(
            inner
                .pointer("/pages/0/page_number")
                .and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            inner
                .pointer("/pages/1/page_number")
                .and_then(|v| v.as_u64()),
            Some(3)
        );
    }
    fn sample_bash(exit_code: i32, output: &[u8], timed_out: bool) -> BashOutput {
        BashOutput {
            output: output.to_vec(),
            output_for_prompt: String::new(),
            exit_code,
            command: "cmd".into(),
            truncated: false,
            signal: None,
            timed_out,
            description: None,
            current_dir: "/tmp".into(),
            output_file: String::new(),
            total_bytes: output.len(),
            output_delta: None,
            was_bare_echo: false,
        }
    }
    fn assert_cer(
        resp: &distill_tool_runtime::ToolChatCompletionResponse,
        stdout: &str,
        exit_code: i32,
        timed_out: bool,
    ) {
        let result = resp.result.as_ref().unwrap();
        let cer = result.code_execution_result.as_ref().unwrap();
        assert_eq!(cer.stdout, stdout);
        assert!(cer.stderr.is_empty());
        assert_eq!(cer.exit_code, exit_code);
        assert_eq!(cer.command_timed_out, timed_out);
        assert_eq!(result.sender, "assistant");
        assert_eq!(result.message_tag.as_deref(), Some("raw_function_result"));
    }
    fn bg_started() -> BackgroundTaskStarted {
        BackgroundTaskStarted {
            task_id: "t1".into(),
            task_type: "bash".into(),
            output_file: "/tmp/out".into(),
            status: "running".into(),
            command: "sleep 99".into(),
            summary: "running".into(),
            retrieval_hint: String::new(),
            pre_formatted: None,
            pid: None,
        }
    }
    #[test]
    fn bash_output_chat_completion_carries_exit_and_stdout() {
        let resp = distill_tool_runtime::ToolOutput::chat_completion_output(&sample_bash(
            0, b"hello\n", false,
        ))
        .unwrap();
        assert_cer(&resp, "hello\n", 0, false);
        assert!(resp.result.as_ref().unwrap().extra.is_empty());
    }
    #[test]
    fn bash_output_chat_completion_empty_stdout_still_emits() {
        let resp =
            distill_tool_runtime::ToolOutput::chat_completion_output(&sample_bash(0, b"", false))
                .unwrap();
        assert_cer(&resp, "", 0, false);
    }
    #[test]
    fn bash_output_chat_completion_timeout_and_nonzero_exit() {
        let resp = distill_tool_runtime::ToolOutput::chat_completion_output(&sample_bash(
            124, b"partial", true,
        ))
        .unwrap();
        assert_cer(&resp, "partial", 124, true);
    }
    #[test]
    fn bash_output_chat_completion_lossy_utf8() {
        let resp = distill_tool_runtime::ToolOutput::chat_completion_output(&sample_bash(
            1,
            &[0x66, 0x6f, 0x6f, 0xff, 0x62, 0x61, 0x72],
            false,
        ))
        .unwrap();
        let stdout = &resp
            .result
            .as_ref()
            .unwrap()
            .code_execution_result
            .as_ref()
            .unwrap()
            .stdout;
        assert!(stdout.starts_with("foo"));
        assert!(stdout.ends_with("bar"));
        assert!(stdout.contains('\u{FFFD}'));
    }
    #[test]
    fn bash_output_chat_completion_truncation_marker_and_extra() {
        let mut bash = sample_bash(0, b"head...tail", false);
        bash.truncated = true;
        bash.total_bytes = 50_000;
        bash.output_file = "/tmp/out.log".into();
        let resp = distill_tool_runtime::ToolOutput::chat_completion_output(&bash).unwrap();
        let result = resp.result.as_ref().unwrap();
        let stdout = &result.code_execution_result.as_ref().unwrap().stdout;
        assert!(stdout.starts_with("head...tail"));
        assert!(stdout.contains("[truncated:"));
        assert!(stdout.contains("full output at: /tmp/out.log"));
        assert_eq!(
            result.extra.get("truncated"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(
            result.extra.get("total_bytes"),
            Some(&serde_json::Value::from(50_000u64))
        );
        assert_eq!(
            result.extra.get("output_file"),
            Some(&serde_json::Value::String("/tmp/out.log".into()))
        );
        assert!(
            result
                .code_execution_result
                .as_ref()
                .unwrap()
                .stderr
                .is_empty()
        );
    }
    #[test]
    fn bash_tool_output_foreground_delegates_background_skips() {
        let resp = distill_tool_runtime::ToolOutput::chat_completion_output(&BashToolOutput::Bash(
            sample_bash(0, b"ok", false),
        ))
        .unwrap();
        assert_cer(&resp, "ok", 0, false);
        assert!(
            distill_tool_runtime::ToolOutput::chat_completion_output(
                &BashToolOutput::BackgroundTaskStarted(bg_started())
            )
            .is_none()
        );
    }
    #[test]
    fn aggregate_tool_output_bash_delegates_background_skips() {
        let resp = distill_tool_runtime::ToolOutput::chat_completion_output(&ToolOutput::Bash(
            sample_bash(0, b"agg", false),
        ))
        .unwrap();
        assert_cer(&resp, "agg", 0, false);
        assert!(
            distill_tool_runtime::ToolOutput::chat_completion_output(
                &ToolOutput::BackgroundTaskStarted(bg_started())
            )
            .is_none()
        );
        assert!(
            distill_tool_runtime::ToolOutput::chat_completion_output(&ToolOutput::Text(
                TextOutput::from("noop")
            ))
            .is_none()
        );
    }
    fn sample_edits_applied(old_lines: &[usize]) -> SearchReplaceEditsApplied {
        sample_edits_applied_for("b", old_lines)
    }
    fn sample_edits_applied_for(
        old_string: &str,
        old_lines: &[usize],
    ) -> SearchReplaceEditsApplied {
        SearchReplaceEditsApplied {
            old_string: old_string.into(),
            new_string: "z".into(),
            tool_output_for_prompt: "edited".into(),
            tool_output_for_prompt_concise: None,
            absolute_path: PathBuf::from("/w/a.rs"),
            edits: SearchReplaceEditContextInformation {
                details: old_lines
                    .iter()
                    .map(|&old_line| SearchReplaceEditDetail {
                        old_string: old_string.into(),
                        old_line,
                        new_string: "z".into(),
                        new_line: old_line,
                        context_before: String::new(),
                        context_after: String::new(),
                        line_prefix: String::new(),
                    })
                    .collect(),
            },
            patch: None,
            unicode_normalized: false,
        }
    }
    /// Every applied edit settles the card through the empty success shell;
    /// only a single-match edit of an existing file also carries the anchor.
    fn assert_applied_edit_frame(
        resp: distill_tool_runtime::ToolChatCompletionResponse,
        anchor: Option<distill_tool_runtime::EditFileAnchor>,
    ) {
        let result = resp.result.unwrap();
        assert_eq!(result.message_tag.as_deref(), Some("raw_function_result"));
        assert_eq!(result.edit_file_result, anchor);
        let cer = result.code_execution_result.expect("settle shell");
        assert_eq!(
            (
                cer.stdout.as_str(),
                cer.stderr.as_str(),
                cer.exit_code,
                cer.command_timed_out
            ),
            ("", "", 0, false)
        );
        assert!(result.extra.is_empty());
    }
    #[test]
    fn search_replace_chat_completion_anchors_single_edit() {
        let anchor = Some(distill_tool_runtime::EditFileAnchor { start_line: 137 });
        let applied = SearchReplaceOutput::EditsApplied(sample_edits_applied(&[137]));
        assert_applied_edit_frame(
            distill_tool_runtime::ToolOutput::chat_completion_output(&applied).unwrap(),
            anchor,
        );
        assert_applied_edit_frame(
            distill_tool_runtime::ToolOutput::chat_completion_output(&ToolOutput::SearchReplace(
                applied,
            ))
            .unwrap(),
            anchor,
        );
    }
    #[test]
    fn search_replace_chat_completion_settles_multi_match_and_creation_without_anchor() {
        assert_applied_edit_frame(
            distill_tool_runtime::ToolOutput::chat_completion_output(&ToolOutput::SearchReplace(
                SearchReplaceOutput::EditsApplied(sample_edits_applied(&[2, 9])),
            ))
            .unwrap(),
            None,
        );
        assert_applied_edit_frame(
            distill_tool_runtime::ToolOutput::chat_completion_output(
                &SearchReplaceOutput::EditsApplied(sample_edits_applied_for("", &[1])),
            )
            .unwrap(),
            None,
        );
    }
    #[test]
    fn search_replace_chat_completion_skips_failures() {
        assert!(
            distill_tool_runtime::ToolOutput::chat_completion_output(&ToolOutput::SearchReplace(
                SearchReplaceOutput::MultipleMatchesFound("two".into())
            ))
            .is_none()
        );
    }
    fn sample_run_result(output: ToolOutput) -> ToolRunResult {
        ToolRunResult {
            prompt_text: "prompt".into(),
            effective_tool_name: None,
            output,
        }
    }
    fn bash_tool_id() -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new("bash").unwrap()
    }
    #[test]
    fn into_typed_tool_output_preserves_bash_foreground_cco() {
        let run = sample_run_result(ToolOutput::Bash(sample_bash(0, b"hello-cco", false)));
        let expected_value = serde_json::to_value(&run).unwrap();
        let typed = run.into_typed_tool_output(bash_tool_id());
        assert_eq!(typed.value, expected_value);
        assert_eq!(typed.tool_id, bash_tool_id());
        assert_cer(
            typed
                .chat_completion_output
                .as_ref()
                .expect("bash foreground must preserve chat_completion_output"),
            "hello-cco",
            0,
            false,
        );
        let dropped = distill_tool_runtime::TypedToolOutput::from_value(typed.tool_id, expected_value);
        assert!(dropped.chat_completion_output.is_none());
    }
    #[test]
    fn into_typed_tool_output_non_bash_cco_is_none() {
        let run = sample_run_result(ToolOutput::Text(TextOutput::from("noop")));
        let typed = run.into_typed_tool_output(distill_tool_protocol::ToolId::new("text").unwrap());
        assert!(typed.chat_completion_output.is_none());
    }
    #[test]
    fn into_typed_tool_output_background_task_cco_is_none() {
        let run = sample_run_result(ToolOutput::BackgroundTaskStarted(bg_started()));
        let typed = run.into_typed_tool_output(bash_tool_id());
        assert!(typed.chat_completion_output.is_none());
    }
    #[test]
    fn typed_tool_output_preserving_cco_serialize_failure_yields_null_value() {
        struct AlwaysFailSerialize;
        impl Serialize for AlwaysFailSerialize {
            fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("intentional serialize failure"))
            }
        }
        let bash = ToolOutput::Bash(sample_bash(0, b"kept-cco", false));
        let typed = typed_tool_output_preserving_cco(bash_tool_id(), &AlwaysFailSerialize, &bash);
        assert_eq!(typed.value, serde_json::Value::Null);
        assert_cer(
            typed
                .chat_completion_output
                .as_ref()
                .expect("CCO still attached when payload serialize fails"),
            "kept-cco",
            0,
            false,
        );
    }
}
