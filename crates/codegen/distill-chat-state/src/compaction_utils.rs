// Modified for Distill by Samuel Fajreldines, 2026.
//! Pure utility functions and types for compaction support.
//!
//! These are stateless functions that operate on conversation data only —
//! no I/O, no actor state. They live in `distill-chat-state` so that both
//! this crate and `distill-shell` can share them without duplication.
use std::collections::BTreeSet;
use distill_sampling_types::{ContentPart, ConversationItem, SyntheticReason, ToolResultItem};
pub const AGENT_MESSAGE_MODEL_LABEL: &str =
    "[Message authored by another agent; not a human request or approval.]";
/// Canonical history prepared exactly once for a model-facing request.
/// The private inner value distinguishes prepared history without inspecting payload text.
pub struct ModelRequestHistory(Vec<ConversationItem>);
impl ModelRequestHistory {
    pub fn from_raw(conversation: Vec<ConversationItem>) -> Self {
        Self(
            conversation
                .into_iter()
                .map(|item| match item {
                    ConversationItem::User(mut user)
                        if user.synthetic_reason == SyntheticReason::AgentMessage =>
                    {
                        user.content.insert(
                            0,
                            ContentPart::Text {
                                text: std::sync::Arc::<str>::from(AGENT_MESSAGE_MODEL_LABEL),
                            },
                        );
                        ConversationItem::User(user)
                    }
                    other => other,
                })
                .collect(),
        )
    }
    pub fn into_items(self) -> Vec<ConversationItem> {
        self.0
    }
}
/// Drops tool results and backend tool calls, flattening assistant `tool_calls` into text.
/// Mutates assistant text in place — do not send this to a provider that validates signed `reasoning`.
/// Use [`prepare_conversation_for_summarization`], which also strips `reasoning`.
pub(crate) fn strip_tool_messages_for_conversation_item(
    conversation: Vec<ConversationItem>,
) -> Vec<ConversationItem> {
    conversation
        .into_iter()
        .filter_map(|item| match item {
            ConversationItem::ToolResult(_) => None,
            ConversationItem::BackendToolCall(_) => None,
            ConversationItem::Assistant(mut a) => {
                if !a.tool_calls.is_empty() {
                    let tool_names: Vec<String> =
                        a.tool_calls.iter().map(|tc| tc.name.clone()).collect();
                    let tool_info = format!("\n[Called tools: {}]", tool_names.join(", "));
                    a.content = if a.content.is_empty() {
                        std::sync::Arc::<str>::from(tool_info)
                    } else {
                        let mut s = String::with_capacity(a.content.len() + tool_info.len());
                        s.push_str(&a.content);
                        s.push_str(&tool_info);
                        std::sync::Arc::<str>::from(s)
                    };
                    a.tool_calls.clear();
                }
                Some(ConversationItem::Assistant(a))
            }
            other => Some(other),
        })
        .collect()
}
/// Drops every `ConversationItem::Reasoning(_)` sibling.
/// Required before backends that reject structured reasoning, and before summarization.
pub fn strip_reasoning_blocks(conversation: Vec<ConversationItem>) -> Vec<ConversationItem> {
    conversation
        .into_iter()
        .filter(|item| !matches!(item, ConversationItem::Reasoning(_)))
        .collect()
}
/// Replace `ContentPart::Image` entries with `"[image]"` so downstream
/// consumers (summary model, segment store) don't carry megabytes of base64.
pub(crate) fn strip_images(conversation: Vec<ConversationItem>) -> Vec<ConversationItem> {
    conversation
        .into_iter()
        .map(|item| match item {
            ConversationItem::User(mut u) => {
                for part in &mut u.content {
                    if matches!(part, ContentPart::Image { .. }) {
                        *part = ContentPart::Text {
                            text: std::sync::Arc::<str>::from("[image]"),
                        };
                    }
                }
                ConversationItem::User(u)
            }
            other => other,
        })
        .collect()
}
/// Prepare a conversation for a summarization call (compaction or memory flush).
/// Strips tool messages, reasoning, and images. Reasoning must go because text mutation
/// invalidates signed `thinking` blocks, which strict providers reject with a 400.
pub fn prepare_conversation_for_summarization(
    conversation: Vec<ConversationItem>,
) -> Vec<ConversationItem> {
    strip_images(strip_reasoning_blocks(
        strip_tool_messages_for_conversation_item(conversation),
    ))
}
/// Segment-store prep (`segments` mode): keep tool I/O verbatim, strip only images + reasoning.
pub fn prepare_conversation_for_segment(
    conversation: Vec<ConversationItem>,
) -> Vec<ConversationItem> {
    strip_images(strip_reasoning_blocks(conversation))
}
/// Drop a trailing assistant turn whose `tool_calls` lack a `ToolResult` (else strict backends reject the dangling `tool_use`).
pub fn truncate_trailing_incomplete_tool_call(
    mut conversation: Vec<ConversationItem>,
) -> Vec<ConversationItem> {
    while matches!(
        conversation.last(),
        Some(ConversationItem::Assistant(a)) if !a.tool_calls.is_empty()
    ) {
        conversation.pop();
    }
    conversation
}
/// Cache-aligned summarizer prep: keep tool I/O + images so the prefix matches the engine cache; set `strip_reasoning` when the provider rejects mutated thinking blocks.
pub fn prepare_conversation_for_verbatim_summarization(
    conversation: Vec<ConversationItem>,
    strip_reasoning: bool,
) -> Vec<ConversationItem> {
    let conversation = if strip_reasoning {
        strip_reasoning_blocks(conversation)
    } else {
        conversation
    };
    truncate_trailing_incomplete_tool_call(conversation)
}
/// Per-item token estimate via the trigger-side estimator, so `fit`'s budget matches what fired the compaction (counts images + encrypted reasoning).
fn estimate_item_tokens(item: &ConversationItem) -> u64 {
    crate::actor::state::estimate_item_tokens(item)
}
/// Shrink a verbatim conversation to `max_tokens`: drop oldest whole turns (System kept, tool runs unsplit; the last turn is truncated in place rather than dropped).
pub fn fit_conversation_to_budget(
    conversation: Vec<ConversationItem>,
    max_tokens: u64,
) -> Vec<ConversationItem> {
    let total: u64 = conversation.iter().map(estimate_item_tokens).sum();
    if total <= max_tokens {
        return conversation;
    }
    let mut head: Vec<ConversationItem> = Vec::new();
    let mut body: Vec<ConversationItem> = conversation;
    if matches!(body.first(), Some(ConversationItem::System(_))) {
        head.push(body.remove(0));
    }
    let budget = max_tokens.saturating_sub(head.iter().map(estimate_item_tokens).sum::<u64>());
    let mut remaining = budget;
    let mut start = body.len();
    for (i, item) in body.iter().enumerate().rev() {
        let cost = estimate_item_tokens(item);
        if cost > remaining {
            break;
        }
        remaining -= cost;
        start = i;
    }
    while matches!(body.get(start), Some(ConversationItem::ToolResult(_))) {
        start += 1;
    }
    if start < body.len() {
        head.extend(body.into_iter().skip(start));
    } else {
        head.extend(recover_truncated_tail_unit(body, budget));
    }
    head
}
/// Keep the most-recent turn but truncate its content to `budget` (with its owning `tool_use`) instead of dropping it.
fn recover_truncated_tail_unit(
    mut body: Vec<ConversationItem>,
    budget: u64,
) -> Vec<ConversationItem> {
    let mut results: Vec<ConversationItem> = Vec::new();
    while matches!(body.last(), Some(ConversationItem::ToolResult(_))) {
        results.push(body.pop().expect("last() was Some"));
    }
    results.reverse();
    if results.is_empty() {
        return match body.pop() {
            Some(item) => vec![truncate_item_to_tokens(item, budget)],
            None => Vec::new(),
        };
    }
    let owner = if matches!(
        body.last(),
        Some(ConversationItem::Assistant(a)) if !a.tool_calls.is_empty()
    ) {
        body.pop()
    } else {
        None
    };
    let owner_cost = owner.as_ref().map(estimate_item_tokens).unwrap_or(0);
    let result_budget = budget.saturating_sub(owner_cost);
    let per = (result_budget / results.len() as u64).max(1);
    let mut unit: Vec<ConversationItem> = Vec::new();
    if let Some(o) = owner {
        unit.push(o);
    }
    unit.extend(results.into_iter().map(|r| truncate_item_to_tokens(r, per)));
    unit
}
/// Truncate one item's content text to at most `max_tokens`, appending a `[... truncated N bytes ...]` marker (structural fields kept).
fn truncate_item_to_tokens(item: ConversationItem, max_tokens: u64) -> ConversationItem {
    let max_bytes = (max_tokens as usize).saturating_mul(4);
    match item {
        ConversationItem::ToolResult(mut t) => {
            if let Some(s) = truncate_text_to_bytes(&t.content, max_bytes) {
                t.content = s;
            }
            ConversationItem::ToolResult(t)
        }
        ConversationItem::Assistant(mut a) => {
            if let Some(s) = truncate_text_to_bytes(&a.content, max_bytes) {
                a.content = s;
            }
            ConversationItem::Assistant(a)
        }
        ConversationItem::User(mut u) => {
            for part in &mut u.content {
                if let ContentPart::Text { text } = part
                    && let Some(s) = truncate_text_to_bytes(text, max_bytes)
                {
                    *text = s;
                }
            }
            ConversationItem::User(u)
        }
        other => other,
    }
}
/// Char-boundary-safe prefix of `s` (incl. truncation marker) within `max_bytes`; `None` if `s` already fits.
fn truncate_text_to_bytes(s: &str, max_bytes: usize) -> Option<std::sync::Arc<str>> {
    if s.len() <= max_bytes {
        return None;
    }
    const MARKER_RESERVE: usize = 64;
    let keep = max_bytes.saturating_sub(MARKER_RESERVE);
    let mut end = keep.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = s.len() - end;
    Some(std::sync::Arc::<str>::from(format!(
        "{}\n[... truncated {dropped} bytes to fit the compaction window ...]",
        s.get(..end)?
    )))
}
/// Tags injected by the runtime that should be stripped from user queries.
const SYSTEM_TAGS: &[&str] = &[
    "user_info",
    "project_layout",
    "git_status",
    "fork-context",
    "system-reminder",
    "agent-memory",
    "system_reminder",
    "background_context",
    "command-name",
    "command-message",
    "command-args",
    "rules",
];
/// Strip all known system/metadata tag blocks from `text`.
/// Unclosed tags are left untouched.
fn strip_system_tags(text: &str) -> String {
    let mut result = text.to_string();
    for tag in SYSTEM_TAGS {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        while let Some(start) = result.find(&open) {
            if let Some(rel_end) = result.get(start..).and_then(|rest| rest.find(&close)) {
                let end_pos = start + rel_end + close.len();
                result.replace_range(start..end_pos, "");
            } else {
                break;
            }
        }
    }
    result.trim().to_string()
}
/// Extracts the user query from a message that may contain metadata tags.
/// Prefers `<user_query>` content; otherwise strips known metadata tags and returns the rest.
pub fn extract_user_query(text: &str) -> String {
    let stripped = strip_system_tags(text);
    if let Some(start) = stripped.find("<user_query>") {
        let content_start = start + "<user_query>".len();
        if let Some(end) = stripped
            .get(content_start..)
            .and_then(|rest| rest.find("</user_query>"))
        {
            return stripped
                .get(content_start..content_start + end)
                .unwrap_or("")
                .trim()
                .to_string();
        }
    }
    stripped
}
/// Extract the last actual user query text (stripping metadata tags).
/// Walks backward to the last `User` item and extracts via [`extract_user_query`].
pub fn extract_last_user_query(conversation: &[ConversationItem]) -> Option<String> {
    conversation
        .iter()
        .rev()
        .find(|item| matches!(item, ConversationItem::User(_)))
        .map(|item| extract_user_query(&item.text_content()))
        .filter(|q| !q.is_empty())
}
/// The continuation prompt added after auto-compaction.
/// Stored here so query-extraction helpers can exclude it without a circular dependency
/// or a second hard-coded copy.
pub const AUTO_CONTINUE_PROMPT: &str = r#"Continue the conversation from where it left off without asking the user any further questions. Resume directly - do not acknowledge the summary, do not recap what was happening, do not preface with "I'll continue" or similar.
Pick up the last task as if the break never happened."#;
/// `false` twin: no preset in this build injects a bootstrap note.
fn is_bootstrap_reminder_text(_text: &str) -> bool {
    false
}
/// True when extracted query text is a synthetic session-internal turn, not a human prompt.
/// Covers empty metadata-only items, the `__auto_continue__` sentinel, [`AUTO_CONTINUE_PROMPT`],
/// and a `<system_reminder>` bootstrap note.
pub fn is_synthetic_extracted_query(text: &str) -> bool {
    text.is_empty()
        || text == "__auto_continue__"
        || text == AUTO_CONTINUE_PROMPT
        || is_bootstrap_reminder_text(text)
}
/// Classify whether a `ConversationItem` is a real user turn for compaction.
/// Not real if it is non-`User`, has `synthetic_reason`, or its extracted text is synthetic.
/// Image-only prompts ARE real — they must anchor the boundary even with no text.
pub fn is_real_user_turn(item: &ConversationItem) -> bool {
    match item {
        ConversationItem::User(u) => {
            if !u.synthetic_reason.is_human() {
                return false;
            }
            let has_images = u
                .content
                .iter()
                .any(|p| matches!(p, ContentPart::Image { .. }));
            if has_images {
                return true;
            }
            let extracted = extract_user_query(&item.text_content());
            !is_synthetic_extracted_query(&extracted)
        }
        _ => false,
    }
}
/// Extract all real user queries, in order ([`is_real_user_turn`]).
/// Used where human-authored prompts must not be polluted by synthetics or compaction artifacts.
pub fn extract_real_user_queries(conversation: &[ConversationItem]) -> Vec<String> {
    conversation
        .iter()
        .filter(|item| is_real_user_turn(item))
        .map(|item| extract_user_query(&item.text_content()))
        .collect()
}
/// Extract the last real user query text, skipping synthetic turns.
/// Unlike [`extract_last_user_query`], this returns only content the user actually typed.
/// `None` when no real user query is found.
pub fn extract_last_real_user_query(conversation: &[ConversationItem]) -> Option<String> {
    conversation
        .iter()
        .rev()
        .find(|item| is_real_user_turn(item))
        .map(|item| extract_user_query(&item.text_content()))
}
/// Extract messages since the last user message. Tool results are placeholder-replaced.
/// Uses the raw `User` boundary, which includes synthetics.
/// For compaction, prefer [`extract_messages_since_last_real_user`].
pub fn extract_messages_since_last_user(
    conversation: &[ConversationItem],
) -> Vec<ConversationItem> {
    let mut messages: Vec<_> = conversation
        .iter()
        .rev()
        .take_while(|item| !matches!(item, ConversationItem::User(_)))
        .filter_map(|item| match item {
            ConversationItem::Assistant(a) => Some(ConversationItem::Assistant(a.clone())),
            ConversationItem::ToolResult(t) => Some(ConversationItem::ToolResult(ToolResultItem {
                tool_call_id: t.tool_call_id.clone(),
                content: std::sync::Arc::<str>::from("Tool call omitted..."),
                images: Vec::new(),
            })),
            _ => None,
        })
        .collect();
    messages.reverse();
    messages
}
/// Extract messages since the last real user turn. Synthetics do not reset the boundary.
/// Prevents compaction from splitting an assistant/tool pair across a synthetic injection
/// (which would orphan a `ToolResult`). Tool results are placeholder-replaced.
pub fn extract_messages_since_last_real_user(
    conversation: &[ConversationItem],
) -> Vec<ConversationItem> {
    let boundary_idx = conversation.iter().rposition(is_real_user_turn);
    let start = match boundary_idx {
        Some(idx) => idx + 1,
        None => 0,
    };
    conversation
        .iter()
        .skip(start)
        .filter_map(|item| match item {
            ConversationItem::Assistant(a) => Some(ConversationItem::Assistant(a.clone())),
            ConversationItem::ToolResult(t) => Some(ConversationItem::ToolResult(ToolResultItem {
                tool_call_id: t.tool_call_id.clone(),
                content: std::sync::Arc::<str>::from("Tool call omitted..."),
                images: Vec::new(),
            })),
            _ => None,
        })
        .collect()
}
#[derive(Clone, Copy)]
enum AgentMessagePosition {
    Only,
    BeforeHuman,
    AfterHuman,
}
/// Latest raw agent-authored anchor and its structural order relative to human input.
#[derive(Clone)]
pub struct AgentMessageAnchor {
    item: ConversationItem,
    position: AgentMessagePosition,
}
fn extract_latest_agent_message(conversation: &[ConversationItem]) -> Option<AgentMessageAnchor> {
    let agent_message_index = conversation.iter().rposition(|item| {
        matches!(
            item,
            ConversationItem::User(user)
                if user.synthetic_reason == SyntheticReason::AgentMessage
        )
    })?;
    let position = match conversation.iter().rposition(is_real_user_turn) {
        Some(human_index) if agent_message_index < human_index => AgentMessagePosition::BeforeHuman,
        Some(_) => AgentMessagePosition::AfterHuman,
        None => AgentMessagePosition::Only,
    };
    Some(AgentMessageAnchor {
        item: conversation.get(agent_message_index)?.clone(),
        position,
    })
}
fn extract_messages_since_last_compaction_anchor(
    conversation: &[ConversationItem],
) -> Vec<ConversationItem> {
    let boundary = conversation.iter().rposition(|item| {
        is_real_user_turn(item)
            || matches!(
                item,
                ConversationItem::User(user)
                    if user.synthetic_reason == SyntheticReason::AgentMessage
            )
    });
    let start = boundary.map_or(0, |idx| {
        if matches!(
            conversation.get(idx),
            Some(ConversationItem::User(user))
                if user.synthetic_reason == SyntheticReason::AgentMessage
        ) {
            idx
        } else {
            idx + 1
        }
    });
    conversation
        .iter()
        .skip(start)
        .filter_map(|item| match item {
            ConversationItem::User(user)
                if user.synthetic_reason == SyntheticReason::AgentMessage =>
            {
                Some(ConversationItem::User(user.clone()))
            }
            ConversationItem::Assistant(assistant) => {
                Some(ConversationItem::Assistant(assistant.clone()))
            }
            ConversationItem::ToolResult(result) => {
                Some(ConversationItem::ToolResult(ToolResultItem {
                    tool_call_id: result.tool_call_id.clone(),
                    content: std::sync::Arc::<str>::from("Tool call omitted..."),
                    images: Vec::new(),
                }))
            }
            _ => None,
        })
        .collect()
}
/// Summary of a running subagent for compaction context.
/// Compaction-layer type; mapped from the protocol type in `run_compact_inner()`.
#[derive(Clone)]
pub struct RunningSubagentSummary {
    /// The subagent's unique ID.
    pub subagent_id: String,
    /// The agent type name (e.g. "Explore", "general-purpose").
    pub subagent_type: String,
    /// Human-readable description of what the subagent is doing.
    pub description: String,
    /// Wall-clock elapsed time since the subagent was spawned, in milliseconds.
    pub elapsed_ms: u64,
}
/// Summary of a running background task for compaction context.
#[derive(Clone)]
pub struct BackgroundTaskSummary {
    pub task_id: String,
    pub command: String,
    pub status: String,
    /// Model-facing name of the tool that created this task (e.g. `monitor`).
    /// `None` omits it from the reminder.
    pub tool_name: Option<String>,
}
/// Summary of a still-registered scheduled loop for compaction context.
#[derive(Clone)]
pub struct ScheduledLoopSummary {
    pub task_id: String,
    pub interval: String,
    pub next_fire_at: String,
    pub prompt: String,
    pub recurring: bool,
    pub durable: bool,
}
/// Summary of a still-live workflow run for compaction context.
#[derive(Clone)]
pub struct WorkflowRunSummary {
    pub name: String,
    pub run_id: String,
    pub status: String,
    pub objective: String,
    pub current_phase: Option<String>,
    pub agents_used: u64,
    pub agent_budget: Option<u64>,
    pub elapsed_ms: u64,
}
/// Summary of a connected MCP server for compaction context.
#[derive(Clone)]
pub struct CompactionServerSummary {
    pub name: String,
    pub tool_count: usize,
    pub description: Option<String>,
}
/// A dependency-free mirror of `TodoStatus` (distill-tools), kept here so
/// this crate avoids that heavy dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoSummaryStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}
impl TodoSummaryStatus {
    pub fn is_actionable(self) -> bool {
        matches!(self, Self::Pending | Self::InProgress)
    }
    /// Mirrors `TodoStatus::tag()` in distill-tools.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Pending => "[pending]",
            Self::InProgress => "[in_progress]",
            Self::Completed => "[completed]",
            Self::Cancelled => "[cancelled]",
        }
    }
}
/// Compaction-layer summary of a todo item. Protocol-layer equivalent is
/// `TodoItem` in distill-tools.
#[derive(Clone)]
pub struct TodoSummary {
    pub id: String,
    pub content: String,
    pub status: TodoSummaryStatus,
}
/// Context captured at compaction time. Pure data.
/// Rendering into system-reminder format is the consumer's job.
pub struct CompactionStateContext {
    /// Monotonic cwd generation; zero preserves the legacy compaction shape.
    pub cwd_generation: u64,
    /// Project instructions resolved for the latest destination cwd.
    pub destination_project_instructions: Option<String>,
    /// Latest agent-authored input and its order relative to the latest human turn.
    pub agent_message_anchor: Option<AgentMessageAnchor>,
    /// Messages since the latest human or agent-message compaction anchor.
    /// Runtime synthetic injections do not reset the boundary.
    pub recent_messages: Vec<ConversationItem>,
    /// The last real user query text (skips synthetic injections and
    /// auto-continue prompts).
    pub last_user_query: Option<String>,
    /// Files the agent edited this session (from agent_edited_paths).
    pub agent_edited_paths: Vec<String>,
    /// Running background tasks.
    pub running_tasks: Vec<BackgroundTaskSummary>,
    /// Subagents that are still running at compaction time.
    pub running_subagents: Vec<RunningSubagentSummary>,
    /// Connected MCP servers, for post-compaction system-reminder injection.
    pub connected_mcp_servers: Vec<CompactionServerSummary>,
    /// Todo list captured at compaction time, for post-compaction
    /// system-reminder injection.
    pub todos: Vec<TodoSummary>,
    /// Scheduled loops still registered at compaction time.
    pub scheduled_loops: Vec<ScheduledLoopSummary>,
    /// Non-terminal workflow runs at compaction time.
    pub workflows: Vec<WorkflowRunSummary>,
    /// Model-facing workflow tool name, when one could be resolved.
    pub workflow_tool_name: Option<String>,
}
/// Live session state captured at compaction time, fed to
/// [`CompactionStateContext::build`].
#[derive(Default)]
pub struct CompactionInputs {
    pub cwd_generation: u64,
    pub destination_project_instructions: Option<String>,
    pub running_tasks: Vec<BackgroundTaskSummary>,
    pub running_subagents: Vec<RunningSubagentSummary>,
    pub agent_edited_paths: BTreeSet<String>,
    pub connected_mcp_servers: Vec<CompactionServerSummary>,
    pub todos: Vec<TodoSummary>,
    pub scheduled_loops: Vec<ScheduledLoopSummary>,
    pub workflows: Vec<WorkflowRunSummary>,
    pub workflow_tool_name: Option<String>,
    /// When set, used as `last_user_query` so compact cannot revive a stale pre-goal prompt.
    pub goal_objective: Option<String>,
}
impl CompactionStateContext {
    /// Build the state context from current session state.
    /// Uses a typed compaction boundary; `last_user_query` prefers `inputs.goal_objective` when set.
    pub async fn build(conversation: &[ConversationItem], inputs: CompactionInputs) -> Self {
        Self {
            cwd_generation: inputs.cwd_generation,
            destination_project_instructions: inputs.destination_project_instructions,
            agent_message_anchor: extract_latest_agent_message(conversation),
            recent_messages: extract_messages_since_last_compaction_anchor(conversation),
            last_user_query: inputs
                .goal_objective
                .filter(|objective| !objective.trim().is_empty())
                .or_else(|| extract_last_real_user_query(conversation)),
            agent_edited_paths: inputs.agent_edited_paths.into_iter().collect(),
            running_tasks: inputs.running_tasks,
            running_subagents: inputs.running_subagents,
            connected_mcp_servers: inputs.connected_mcp_servers,
            todos: inputs.todos,
            scheduled_loops: inputs.scheduled_loops,
            workflows: inputs.workflows,
            workflow_tool_name: inputs.workflow_tool_name,
        }
    }
    /// Create a task summary from individual fields.
    pub fn task_summary(
        task_id: String,
        command: String,
        status: &str,
        tool_name: Option<String>,
    ) -> BackgroundTaskSummary {
        BackgroundTaskSummary {
            task_id,
            command,
            status: status.to_string(),
            tool_name,
        }
    }
    /// Compaction view: drop the assistant/tool working tail, keep the latest agent-message anchor.
    /// For a single-real-user-turn sub-agent, keeping `recent_messages` frees almost nothing
    /// and re-cues the model to re-read the same files.
    pub fn for_compaction(&self) -> Self {
        Self {
            cwd_generation: self.cwd_generation,
            destination_project_instructions: self.destination_project_instructions.clone(),
            agent_message_anchor: self
                .agent_message_anchor
                .clone()
                .or_else(|| extract_latest_agent_message(&self.recent_messages)),
            recent_messages: Vec::new(),
            last_user_query: self.last_user_query.clone(),
            agent_edited_paths: self.agent_edited_paths.clone(),
            running_tasks: self.running_tasks.clone(),
            running_subagents: self.running_subagents.clone(),
            connected_mcp_servers: self.connected_mcp_servers.clone(),
            todos: self.todos.clone(),
            scheduled_loops: self.scheduled_loops.clone(),
            workflows: self.workflows.clone(),
            workflow_tool_name: self.workflow_tool_name.clone(),
        }
    }
}
/// Clean the compaction model's raw output into the plain-text `Summary:` block.
/// Leading scratchpad is stripped; control tokens echoed in the body are neutralized
/// so they cannot prime the next turn to re-emit a `<summary>` block.
pub fn format_compact_summary(summary: &str) -> String {
    let mut result = summary.to_string();
    while let Some(start) = result.find("<analysis>") {
        let is_leading = match result.find("<summary>") {
            Some(sp) => {
                start < sp
                    || result
                        .get(sp + "<summary>".len()..start)
                        .is_some_and(|s| s.trim().is_empty())
            }
            None => result.get(..start).is_some_and(|s| s.trim().is_empty()),
        };
        if !is_leading {
            break;
        }
        match result
            .get(start..)
            .and_then(|rest| rest.find("</analysis>"))
        {
            Some(rel) => {
                let end = start + rel + "</analysis>".len();
                result = format!(
                    "{}{}",
                    result.get(..start).unwrap_or(""),
                    result.get(end..).unwrap_or("")
                );
            }
            None => {
                let drop_to = result
                    .get(start..)
                    .and_then(|rest| rest.find("<summary>"))
                    .map_or(result.len(), |rel| start + rel);
                result = format!(
                    "{}{}",
                    result.get(..start).unwrap_or(""),
                    result.get(drop_to..).unwrap_or("")
                );
                break;
            }
        }
    }
    if let Some(start) = result.find("<summary>")
        && let Some(end) = result.rfind("</summary>")
        && end > start
    {
        let before = result.get(..start).unwrap_or("").to_string();
        let after = result
            .get(end + "</summary>".len()..)
            .unwrap_or("")
            .to_string();
        let inner = strip_leading_scratchpad(
            result
                .get(start + "<summary>".len()..end)
                .unwrap_or("")
                .trim(),
        );
        result = format!("{before}Summary:\n{inner}{after}");
    }
    result = neutralize_compaction_control_tokens(&result);
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result.trim().to_string()
}
/// Peel leading drafting scratchpad off an extracted `<summary>` block.
/// A markdown "**Analysis**" header has no opening tag; drop through the last `</analysis>`.
/// Skip the peel when the block already starts with a numbered section, so an echo cannot truncate.
fn strip_leading_scratchpad(inner: &str) -> String {
    let mut s = inner.trim();
    let lead = s.trim_start_matches(['#', '*', '-', '>', ' ', '\t']);
    if !lead.starts_with(|c: char| c.is_ascii_digit())
        && let Some(pos) = s.rfind("</analysis>")
    {
        s = s
            .get(pos + "</analysis>".len()..)
            .unwrap_or("")
            .trim_start();
    }
    if let Some(rest) = s.strip_prefix("<summary>") {
        s = rest.trim_start();
    }
    s.to_string()
}
/// Defuse compaction-control tokens echoed inside a summary body.
/// Insert a zero-width space after `<` so they cannot be read as live tags next turn.
/// Closers first so the inserted sentinel never re-matches.
fn neutralize_compaction_control_tokens(text: &str) -> String {
    text.replace("</summary>", "<\u{200b}/summary>")
        .replace("<summary>", "<\u{200b}summary>")
        .replace("</analysis>", "<\u{200b}/analysis>")
        .replace("<analysis>", "<\u{200b}analysis>")
        .replace("</summary_request>", "<\u{200b}/summary_request>")
        .replace("<summary_request>", "<\u{200b}summary_request>")
}
/// Clean tags via [`format_compact_summary`] and prepend the continuation
/// preamble. This is the user message content that replaces the compacted
/// conversation.
pub fn format_compact_summary_content(raw_summary: &str) -> String {
    let cleaned = format_compact_summary(raw_summary);
    format!(
        "This session is being continued from a previous conversation that ran out of context. \
         The summary below covers the earlier portion of the conversation.\n\n{cleaned}"
    )
}
/// Floor for the cleaned seed (degenerate band observed at 75–264
/// chars; smallest healthy prod summary observed at 3,242 chars).
const MIN_SUMMARY_SEED_CHARS: usize = 500;
/// True when the cleaned summary seed is too small to plausibly carry the
/// task state of the conversation it would replace. Callers should
/// retry like a transient failure.
pub fn is_degenerate_summary(raw_summary: &str) -> bool {
    format_compact_summary(raw_summary).chars().count() < MIN_SUMMARY_SEED_CHARS
}
/// Cap (in `char`s) for the rejected-summary text captured on
/// [`CompactionAttempt::summary`].
pub const MAX_CAPTURED_SUMMARY_CHARS: usize = 8_192;
/// Bound captured text for the request artifact: whole when within `max_chars`,
/// else head + tail around an elision marker. Splits on `char` boundaries.
pub fn bound_captured_output(s: &str, max_chars: usize) -> String {
    let total = s.chars().count();
    if total <= max_chars {
        return s.to_string();
    }
    let head = max_chars / 2;
    let tail = max_chars - head;
    let head_str: String = s.chars().take(head).collect();
    let tail_str: String = s.chars().skip(total - tail).collect();
    let elided = total - head - tail;
    format!("{head_str}\n\n…[{elided} chars elided]…\n\n{tail_str}")
}
/// Diagnostics for a single compaction model call (one retry-loop iteration).
/// Persisted on the request artifact so a degraded retry is not bumped invisibly.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompactionAttempt {
    /// 1-based attempt index, cumulative across input-ladder stages.
    pub attempt: u32,
    /// `"success"`, `"degenerate"`, `"deterministic"`, or `"transient"`.
    pub outcome: String,
    /// Raw char count of the content produced this attempt; `0` if none.
    pub summary_chars: u64,
    /// Raw rejected summary text on a degenerate attempt (bounded by
    /// [`bound_captured_output`]). `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Error detail on a failed (`deterministic` / `transient`) attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
/// Render a `<transcript_location>` pointer block.
/// The carrier embeds this so the model can re-read pre-compaction detail instead of carrying the transcript.
/// Spliced right after `</summary_content>`; carries its own leading blank line.
pub fn format_transcript_location(path: &str) -> String {
    format!(
        "\n\n<transcript_location>\n\
         The full, unsummarized transcript of this conversation is saved at:\n{path}\n\
         If you need details that were dropped from the summary above (exact code, \
         error text, file contents, or earlier tool output), read this file to \
         recover them.\n\
         </transcript_location>"
    )
}
/// Wrap text in `<user_query>...</user_query>` tags.
/// Canonical wrapping shared by `distill-chat-state` and `distill-shell`.
pub fn wrap_user_query(text: impl Into<String>) -> String {
    let text = text.into();
    format!("<user_query>\n{text}\n</user_query>")
}
/// Input data for building a compacted conversation history. Plain data, no I/O.
/// The caller generates the summary, renders the optional reminder, and supplies the user-message prefix.
pub struct CompactedHistoryInput<'a> {
    /// The original system message from the conversation.
    pub system_message: ConversationItem,
    /// The user-info / project-layout prefix (not wrapped in `<user_query>`).
    pub user_message_prefix: String,
    /// Pre-rendered AGENTS.md `<system-reminder>` block to re-inject after the
    /// user prefix. `None` means no project instructions to re-inject.
    /// This preserves project instructions verbatim across compaction.
    pub agents_md_reminder: Option<String>,
    /// State context snapshot taken before compaction cleared the conversation.
    pub state_context: &'a CompactionStateContext,
    /// The LLM-generated compaction summary text.
    pub compaction_summary: String,
    /// An optional pre-rendered `<system-reminder>` block to append after the
    /// summary. `None` means no state reminder is appended.
    pub system_reminder: Option<String>,
    /// When `true`, emit the compaction summary *before* recent messages.
    /// When `false` (the default), recent messages come first (distill
    /// ordering).
    pub summary_before_recent: bool,
    /// Pre-built transcript hint appended to the summary. `None` to omit.
    /// Appended to BOTH the carrier and the distill summary.
    pub transcript_hint: Option<String>,
    /// Number of summaries so far for this user query, including the one being built.
    /// Rendered verbatim into the carrier footer. Ignored by the distill path.
    /// Callers that don't track a counter pass `1`.
    pub summary_count: u64,
}
/// `None` twin: the alternate carrier format is not compiled in.
fn summary_before_recent_carrier(_input: &CompactedHistoryInput<'_>) -> Option<String> {
    None
}
/// This is a pure function with no I/O. It mirrors exactly what
/// `run_compact_inner` in `distill-shell` assembles inline, but is
/// independently testable.
pub fn build_compacted_history(input: CompactedHistoryInput<'_>) -> Vec<ConversationItem> {
    let carrier = summary_before_recent_carrier(&input);
    let summary_first = carrier.is_some();
    let summary_item = carrier.map(ConversationItem::user_meta).unwrap_or_else(|| {
        let mut formatted_summary = format_compact_summary_content(&input.compaction_summary);
        if let Some(ref hint) = input.transcript_hint {
            formatted_summary.push_str(hint);
        }
        ConversationItem::user_meta(formatted_summary)
    });
    let mut compacted: Vec<ConversationItem> = vec![
        input.system_message,
        ConversationItem::user_meta(input.user_message_prefix),
    ];
    let project_instructions = if input.state_context.cwd_generation == 0 {
        input.agents_md_reminder.as_ref()
    } else {
        input
            .state_context
            .destination_project_instructions
            .as_ref()
    };
    if let Some(reminder) = project_instructions {
        compacted.push(ConversationItem::project_instructions(reminder.clone()));
    }
    let anchor = input
        .state_context
        .agent_message_anchor
        .clone()
        .filter(|_| {
            !input.state_context.recent_messages.iter().any(|item| {
                matches!(
                    item,
                    ConversationItem::User(user)
                        if user.synthetic_reason == SyntheticReason::AgentMessage
                )
            })
        });
    if let Some(anchor) = anchor
        .as_ref()
        .filter(|anchor| matches!(anchor.position, AgentMessagePosition::BeforeHuman))
    {
        compacted.push(anchor.item.clone());
    }
    if let Some(ref last_query) = input.state_context.last_user_query {
        compacted.push(ConversationItem::user(wrap_user_query(last_query)));
    }
    if let Some(anchor) =
        anchor.filter(|anchor| !matches!(anchor.position, AgentMessagePosition::BeforeHuman))
    {
        compacted.push(anchor.item);
    }
    if summary_first {
        compacted.push(summary_item);
        for msg in input.state_context.recent_messages.iter().cloned() {
            compacted.push(msg);
        }
    } else {
        for msg in input.state_context.recent_messages.iter().cloned() {
            compacted.push(msg);
        }
        compacted.push(summary_item);
    }
    if let Some(ref reminder) = input.system_reminder {
        compacted.push(ConversationItem::system_reminder(reminder.clone()));
    }
    compacted
}
/// Result of sanitizing a compacted conversation history.
pub struct SanitizeResult {
    /// The sanitized conversation items.
    pub items: Vec<ConversationItem>,
    /// `tool_call_id`s that were stripped because no preceding assistant
    /// `tool_calls` entry matched them.
    pub stripped_tool_call_ids: Vec<String>,
}
/// Check that every `ToolResult` has a matching preceding `Assistant.tool_calls[].id`.
/// Returns violating `tool_call_id`s (empty when valid). Read-only.
pub fn validate_compacted_history(items: &[ConversationItem]) -> Vec<String> {
    let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut invalid_ids = Vec::new();
    for item in items {
        match item {
            ConversationItem::Assistant(a) => {
                for tc in &a.tool_calls {
                    seen_ids.insert(&tc.id);
                }
            }
            ConversationItem::ToolResult(tr) if !seen_ids.contains(tr.tool_call_id.as_str()) => {
                invalid_ids.push(tr.tool_call_id.clone());
            }
            _ => {}
        }
    }
    invalid_ids
}
/// Remove orphaned `ToolResult` items that lack a matching preceding assistant call id.
/// Left-to-right: a result whose id is not yet seen is stripped (including result-before-call).
/// Unanswered assistant calls are NOT stripped — that is not the 400 invariant.
pub fn sanitize_compacted_history(items: Vec<ConversationItem>) -> SanitizeResult {
    let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stripped_tool_call_ids = Vec::new();
    let sanitized = items
        .into_iter()
        .filter(|item| match item {
            ConversationItem::Assistant(a) => {
                for tc in &a.tool_calls {
                    seen_ids.insert(tc.id.as_ref().to_owned());
                }
                true
            }
            ConversationItem::ToolResult(tr) => {
                if seen_ids.contains(&tr.tool_call_id) {
                    true
                } else {
                    stripped_tool_call_ids.push(tr.tool_call_id.clone());
                    false
                }
            }
            _ => true,
        })
        .collect();
    SanitizeResult {
        items: sanitized,
        stripped_tool_call_ids,
    }
}
/// What [`repair_history`] changed; all-zero/empty means nothing was rewritten.
#[derive(Debug, Clone, Default)]
pub struct HistoryRepairReport {
    /// Duplicate `ToolResult` entries removed.
    pub duplicates_removed: usize,
    /// `tool_call_id`s of orphaned/displaced `ToolResult`s stripped — the
    /// shape behind "unexpected `tool_use_id` found in `tool_result` blocks".
    pub stripped_tool_result_ids: Vec<String>,
    /// Synthetic `ToolResult`s inserted for unanswered `tool_calls`.
    pub synthetic_results_inserted: usize,
}
impl HistoryRepairReport {
    /// Whether the repair modified the conversation.
    pub fn changed(&self) -> bool {
        self.duplicates_removed > 0
            || !self.stripped_tool_result_ids.is_empty()
            || self.synthetic_results_inserted > 0
    }
}
/// Repair provider tool-pairing violations (orphaned `ToolResult`s 400 on every request).
/// Dedup, strip displaced results, then backfill synthetic results for calls left unanswered.
/// Pure and idempotent.
pub fn repair_history(items: &mut Vec<ConversationItem>) -> HistoryRepairReport {
    let duplicates_removed = distill_sampling_types::dedup_duplicate_tool_results(items);
    let stripped_tool_result_ids = strip_displaced_tool_results(items);
    let synthetic_results_inserted = distill_sampling_types::repair_dangling_tool_calls(
        items,
        distill_sampling_types::DanglingToolCallReason::HarnessHalted {
            class: "history_repair",
        },
    );
    HistoryRepairReport {
        duplicates_removed,
        stripped_tool_result_ids,
        synthetic_results_inserted,
    }
}
/// Strip `ToolResult`s not in the contiguous run immediately after their declaring `Assistant`.
/// Stricter than "matching id anywhere before" because providers require adjacency.
/// Same contiguous-run rule as the other repair passes so they agree on which calls are answered.
pub fn strip_displaced_tool_results(items: &mut Vec<ConversationItem>) -> Vec<String> {
    let mut run_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut stripped = Vec::new();
    items.retain(|item| match item {
        ConversationItem::Assistant(a) => {
            run_ids = a
                .tool_calls
                .iter()
                .map(|tc| tc.id.as_ref().to_owned())
                .collect();
            true
        }
        ConversationItem::ToolResult(tr) => {
            if run_ids.contains(&tr.tool_call_id) {
                true
            } else {
                stripped.push(tr.tool_call_id.clone());
                false
            }
        }
        _ => {
            run_ids.clear();
            true
        }
    });
    stripped
}
#[cfg(test)]
#[path = "compaction_utils_tests.rs"]
mod tests;
