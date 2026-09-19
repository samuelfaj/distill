//! Canonical, extensible tool types.
pub mod definition;
mod ext;
mod glob;
mod grep;
mod read;
mod schema_utils;
pub mod serde_lenient;
mod task;
mod types;
mod web_search;

pub use ext::Extensions;
pub use glob::GlobToolInput;
pub use grep::{
    GrepFileMatch, GrepLineMatch, GrepOutputMode, GrepSearchOutput, GrepToolInput,
    LenientNumberSchema, LenientNumberSchemaMin0, deserialize_lenient_u32,
};
pub use read::{ReadLineCounts, ReadLineRange};
pub use schema_utils::parse_arguments_from_schema_lossy;
pub use serde_lenient::{
    deserialize_lenient_bool, deserialize_lenient_i64, deserialize_lenient_option_bool,
    deserialize_lenient_required_i64, deserialize_lenient_required_u64, deserialize_lenient_u64,
    lenient_bool_from_json, parse_lenient_i64_value, parse_lenient_u64_value,
};
pub use task::{
    BACKGROUND_SUBAGENT_CONTINUE_PARENT_WORK, BUILTIN_SUBAGENTS, BackgroundNoticeNaming,
    BuiltinSubagent, EXPLORE_PROMPT, EXPLORE_SUBAGENT, GENERAL_PURPOSE_PROMPT,
    GENERAL_PURPOSE_SUBAGENT, KillTaskOutput, KillTaskResult, KillTaskToolInput,
    KillTaskToolNaming, MAX_MULTI_WAIT_IDS, MAX_WAIT_BLOCK_MS_DEFAULT, MAX_WAIT_MS_PLACEHOLDER,
    MultiTaskOutputResult, PLAN_PROMPT, PLAN_SUBAGENT, SubagentCapabilityMode,
    SubagentCompletedOutput, SubagentDescriptor, SubagentIsolationMode, SubagentToolNaming,
    TaskOutputOutput, TaskOutputResult, TaskOutputToolInput, TaskOutputToolNaming, TaskToolInput,
    TaskToolNaming, WaitMode, WaitTasksToolInput, WaitTasksToolNaming, build_kill_task_description,
    build_task_description, build_task_output_description, build_wait_tasks_description,
    builtin_subagent_by_name, default_subagent_type, format_resume_footer,
    format_subagent_auto_backgrounded, format_subagent_completed,
    format_subagent_started_background, format_wait_cap_ms, is_not_sentinel, max_wait_block_ms,
    render_tool_access_fragment, resolve_task_ids, sanitize_optional_arg,
    should_continue_parent_work, task_output_waits, task_output_waits_from_json,
};
pub use types::{
    ArgumentType, SchemaType, ToolArgument, ToolDescription, ValidationError, ValidationErrors,
};
pub use web_search::{WebSearchOutput, WebSearchToolInput};
