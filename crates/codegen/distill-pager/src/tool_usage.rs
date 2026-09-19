//! Tool usage statistics aggregation for the pager.
//!
//! Stats are computed over visible scrollback blocks only (ToolCallBlock variants).
//! Thinking blocks and non-tool RenderBlock variants are excluded.
//! Time tracking is deferred.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::time::Instant;

use ratatui::style::Color;

use crate::scrollback::blocks::tool::ToolCallBlock;
use crate::scrollback::state::ScrollbackState;
use crate::theme::Theme;

/// Block category for stats aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolCategory {
    /// Shell command execution (run_terminal_cmd, bash).
    Execute,
    /// File read operations (read_file).
    Read,
    /// File edits (search_replace, write, apply_patch).
    Edit,
    /// Search/grep operations (grep, glob).
    Search,
    /// Skill invocations (user-defined skills via the Skill tool).
    Skill,
    /// Directory listing (list_dir, ls).
    ListDir,
    /// Web fetch (URL content retrieval).
    WebFetch,
    /// Web search (web search with citations).
    WebSearch,
    /// Other/unknown tool types.
    Other,
    /// Agent thinking/reasoning.
    Thinking,
    /// Agent response message.
    Message,
}

impl ToolCategory {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Execute => "Execute",
            Self::Read => "Read",
            Self::Edit => "Edit",
            Self::Search => "Search",
            Self::Skill => "Skill",
            Self::ListDir => "ListDir",
            Self::WebFetch => "Fetch",
            Self::WebSearch => "WebSearch",
            Self::Other => "Other",
            Self::Thinking => "Thinking",
            Self::Message => "Message",
        }
    }

    /// Compact symbol for sequence bar (single char, no emoji).
    pub fn symbol(&self) -> char {
        match self {
            Self::Execute => '█',
            Self::Read => '▓',
            Self::Edit => '▒',
            Self::Search => '░',
            Self::Skill => crate::glyphs::diamond_filled_char(),
            Self::ListDir => '▀',
            Self::WebFetch => '▄',
            Self::WebSearch => '○',
            Self::Other => crate::glyphs::diamond_filled_char(),
            Self::Thinking => crate::glyphs::diamond_hollow_char(),
            Self::Message => '▪',
        }
    }

    pub fn color(&self, theme: &Theme) -> Color {
        match self {
            Self::Execute => theme.command,
            Self::Read => theme.accent_system,
            Self::Edit => theme.accent_success,
            Self::Search => theme.running,
            Self::Skill => theme.accent_skill,
            Self::ListDir => theme.accent_model,
            Self::WebFetch => theme.accent_tool,
            Self::WebSearch => theme.accent_tool,
            Self::Other => theme.path,
            Self::Thinking => theme.accent_running,
            Self::Message => theme.text_primary,
        }
    }

    pub fn from_tool_block(tc: &ToolCallBlock) -> Self {
        match tc {
            ToolCallBlock::Execute(_) => Self::Execute,
            ToolCallBlock::Read(_) => Self::Read,
            ToolCallBlock::Edit(_) => Self::Edit,
            ToolCallBlock::Search(_) => Self::Search,
            ToolCallBlock::Skill(_) => Self::Skill,
            ToolCallBlock::ListDir(_) => Self::ListDir,
            ToolCallBlock::WebFetch(_) => Self::WebFetch,
            ToolCallBlock::WebSearch(_) => Self::WebSearch,
            ToolCallBlock::IntegrationSearch(_) | ToolCallBlock::UseTool(_) => Self::Other,
            ToolCallBlock::MemorySearch(_)
            | ToolCallBlock::SentMessage(_)
            | ToolCallBlock::Other(_) => Self::Other,
        }
    }
}

// BlockStatus: status dimension (not category)

/// Status of a tool block at aggregation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum BlockStatus {
    /// Tool completed successfully.
    #[default]
    Success,
    /// Tool failed with an error.
    Failed,
    /// Tool completed without a confirmed success or rejection.
    Unconfirmed,
    /// Tool is currently running/streaming.
    Running,
}

/// A single entry in the activity lineage (ordered timeline).
#[derive(Debug, Clone)]
pub struct LineageEntry {
    pub category: ToolCategory,
    /// Duration in ms (None if unknown/pre-completed).
    pub duration_ms: Option<i64>,
    pub running: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CategoryStats {
    /// Number of operations in this category (all statuses).
    pub count: usize,
    pub by_status: HashMap<BlockStatus, usize>,
    /// Scrollback entry indices where this category appeared, used to build the sequence strip.
    pub sequence_positions: Vec<usize>,
    /// Total elapsed time in ms for this category.
    pub total_time_ms: i64,
}

impl CategoryStats {
    pub fn failed_count(&self) -> usize {
        *self.by_status.get(&BlockStatus::Failed).unwrap_or(&0)
    }

    /// Number of operations whose delivery outcome is unconfirmed.
    pub fn unconfirmed_count(&self) -> usize {
        *self.by_status.get(&BlockStatus::Unconfirmed).unwrap_or(&0)
    }

    pub fn running_count(&self) -> usize {
        *self.by_status.get(&BlockStatus::Running).unwrap_or(&0)
    }

    pub fn percent_of(&self, total: usize) -> f64 {
        if total == 0 {
            return 0.0;
        }
        (self.count as f64 / total as f64) * 100.0
    }

    pub fn format_time(&self) -> String {
        if self.total_time_ms == 0 {
            return "-".to_string();
        }
        let secs = self.total_time_ms / 1000;
        let ms = self.total_time_ms % 1000;
        if secs == 0 {
            format!("{}ms", ms)
        } else if secs < 60 {
            format!("{}.{:03}s", secs, ms)
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            crate::views::dock::fmt_elapsed(secs as u64)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StatsScope {
    /// All visible scrollback entries for the current agent session.
    #[default]
    Session,
    /// The turn returned by `ScrollbackState::current_turn()`, with fallback to the latest turn if no current turn exists.
    SelectedTurn,
}

#[derive(Debug, Clone)]
pub struct ToolUsageStats {
    /// Per-category stats (includes Thinking and Message).
    pub categories: BTreeMap<ToolCategory, CategoryStats>,
    /// Total operations across all categories.
    pub total_operations: usize,
    pub computed_at: Instant,
    pub scope: StatsScope,
    /// Ordered timeline of all blocks with category and duration.
    pub lineage: Vec<LineageEntry>,
}

impl Default for ToolUsageStats {
    fn default() -> Self {
        Self {
            categories: BTreeMap::new(),
            total_operations: 0,
            computed_at: Instant::now(),
            scope: StatsScope::default(),
            lineage: Vec::new(),
        }
    }
}

impl ToolUsageStats {
    /// Aggregate stats from entire visible scrollback (session scope).
    pub fn from_scrollback(scrollback: &ScrollbackState) -> Self {
        let n = scrollback.len();
        Self::from_range(scrollback, 0..n, StatsScope::Session)
    }

    /// Aggregate stats from a specific turn; an out-of-range `turn_index` returns empty stats.
    pub fn from_turn(scrollback: &ScrollbackState, turn_index: usize) -> Self {
        match scrollback.turn(turn_index) {
            Some(turn) => Self::from_range(scrollback, turn.range(), StatsScope::SelectedTurn),
            None => ToolUsageStats {
                scope: StatsScope::SelectedTurn,
                computed_at: Instant::now(),
                ..Default::default()
            },
        }
    }

    /// Aggregate stats from the selected turn if any, otherwise the latest turn; empty stats if no turns exist.
    pub fn from_selected_or_latest(scrollback: &ScrollbackState) -> Self {
        let turn_idx = scrollback.current_turn().or_else(|| {
            let count = scrollback.turns().len();
            if count > 0 { Some(count - 1) } else { None }
        });

        match turn_idx {
            Some(idx) => Self::from_turn(scrollback, idx),
            None => ToolUsageStats {
                scope: StatsScope::SelectedTurn,
                computed_at: Instant::now(),
                ..Default::default()
            },
        }
    }

    /// Core aggregation logic over a range of entry indices.
    fn from_range(scrollback: &ScrollbackState, range: Range<usize>, scope: StatsScope) -> Self {
        let mut stats = ToolUsageStats {
            scope,
            computed_at: Instant::now(),
            ..Default::default()
        };

        for idx in range.clone() {
            let Some(entry) = scrollback.entry(idx) else {
                continue;
            };

            match &entry.block {
                crate::scrollback::block::RenderBlock::ToolCall(tc) => {
                    let category = ToolCategory::from_tool_block(tc);
                    let status = if entry.is_running {
                        BlockStatus::Running
                    } else if matches!(tc, ToolCallBlock::SentMessage(message) if message.is_unconfirmed())
                    {
                        BlockStatus::Unconfirmed
                    } else if Self::tool_block_is_failure(tc) {
                        BlockStatus::Failed
                    } else {
                        BlockStatus::Success
                    };

                    let elapsed = Self::tool_block_elapsed_ms(tc);
                    let cat_stats = stats.categories.entry(category).or_default();
                    cat_stats.count += 1;
                    *cat_stats.by_status.entry(status).or_default() += 1;
                    cat_stats.sequence_positions.push(idx);
                    if let Some(ms) = elapsed {
                        cat_stats.total_time_ms += ms;
                    }

                    stats.lineage.push(LineageEntry {
                        category,
                        duration_ms: elapsed,
                        running: entry.is_running,
                    });
                }
                crate::scrollback::block::RenderBlock::Thinking(thinking) => {
                    let elapsed = thinking.elapsed_time_ms();
                    let cat_stats = stats.categories.entry(ToolCategory::Thinking).or_default();
                    cat_stats.count += 1;
                    let status = if entry.is_running {
                        BlockStatus::Running
                    } else {
                        BlockStatus::Success
                    };
                    *cat_stats.by_status.entry(status).or_default() += 1;
                    cat_stats.sequence_positions.push(idx);
                    if let Some(ms) = elapsed {
                        cat_stats.total_time_ms += ms;
                    }

                    stats.lineage.push(LineageEntry {
                        category: ToolCategory::Thinking,
                        duration_ms: elapsed,
                        running: entry.is_running,
                    });
                }
                crate::scrollback::block::RenderBlock::AgentMessage(_) => {
                    let cat_stats = stats.categories.entry(ToolCategory::Message).or_default();
                    cat_stats.count += 1;
                    let status = if entry.is_running {
                        BlockStatus::Running
                    } else {
                        BlockStatus::Success
                    };
                    *cat_stats.by_status.entry(status).or_default() += 1;
                    cat_stats.sequence_positions.push(idx);

                    stats.lineage.push(LineageEntry {
                        category: ToolCategory::Message,
                        duration_ms: None,
                        running: entry.is_running,
                    });
                }
                _ => {}
            }
        }

        stats.total_operations = stats.categories.values().map(|c| c.count).sum();

        stats
    }

    /// Check if a tool block completed with a definite failure.
    fn tool_block_is_failure(tc: &ToolCallBlock) -> bool {
        match tc {
            ToolCallBlock::Execute(b) => !b.is_success(),
            ToolCallBlock::Read(b) => !b.is_success(),
            ToolCallBlock::Edit(b) => !b.is_success(),
            ToolCallBlock::Search(b) => !b.is_success(),
            ToolCallBlock::ListDir(b) => !b.is_success(),
            ToolCallBlock::WebFetch(b) => !b.is_success(),
            ToolCallBlock::WebSearch(b) => !b.is_success(),
            ToolCallBlock::IntegrationSearch(b) => !b.is_success(),
            ToolCallBlock::UseTool(b) => !b.is_success(),
            ToolCallBlock::MemorySearch(b) => !b.is_success(),
            ToolCallBlock::SentMessage(b) => b.is_failure(),
            ToolCallBlock::Skill(b) => !b.is_success(),
            ToolCallBlock::Other(b) => !b.is_success(),
        }
    }

    fn tool_block_elapsed_ms(tc: &ToolCallBlock) -> Option<i64> {
        match tc {
            ToolCallBlock::Execute(b) => b.elapsed_ms(),
            ToolCallBlock::Read(b) => b.elapsed_ms(),
            ToolCallBlock::Edit(b) => b.elapsed_ms(),
            ToolCallBlock::Search(b) => b.elapsed_ms(),
            ToolCallBlock::ListDir(b) => b.elapsed_ms(),
            ToolCallBlock::WebFetch(b) => b.elapsed_ms(),
            ToolCallBlock::WebSearch(b) => b.elapsed_ms(),
            ToolCallBlock::IntegrationSearch(b) => b.elapsed_ms(),
            ToolCallBlock::UseTool(b) => b.elapsed_ms(),
            ToolCallBlock::MemorySearch(b) => b.elapsed_ms(),
            ToolCallBlock::SentMessage(b) => b.elapsed_ms(),
            ToolCallBlock::Skill(b) => b.elapsed_ms(),
            ToolCallBlock::Other(b) => b.elapsed_ms(),
        }
    }

    /// Get categories sorted by count (descending).
    pub fn sorted_categories(&self) -> Vec<(ToolCategory, &CategoryStats)> {
        let mut items: Vec<_> = self.categories.iter().map(|(k, v)| (*k, v)).collect();
        items.sort_by(|a, b| b.1.count.cmp(&a.1.count));
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrollback::block::RenderBlock;
    use crate::scrollback::blocks::tool::{
        EditToolCallBlock, ExecuteToolCallBlock, ReadToolCallBlock, ToolCallBlock,
    };
    use crate::scrollback::state::ScrollbackState;

    #[test]
    fn test_category_stats_percent() {
        let stats = CategoryStats {
            count: 10,
            ..Default::default()
        };
        assert_eq!(stats.percent_of(100), 10.0);
        assert_eq!(stats.percent_of(0), 0.0);
    }

    #[test]
    fn unconfirmed_sent_message_is_not_counted_as_failed() {
        let mut scrollback = ScrollbackState::new();
        scrollback.push_block(RenderBlock::ToolCall(ToolCallBlock::SentMessage(
            crate::scrollback::blocks::tool::SentMessageToolCallBlock::new(
                crate::scrollback::blocks::tool::SentMessagePresentation::Unconfirmed {
                    reason: "delivery could not be confirmed".into(),
                },
                Some(crate::scrollback::blocks::tool::SentMessageInput {
                    target: crate::scrollback::blocks::tool::SentMessageTarget::Unresolved {
                        subagent_id: "sub-1".into(),
                    },
                    delivery: Some(crate::scrollback::blocks::tool::SentMessageDelivery::Steer),
                    text: "follow up".into(),
                }),
            ),
        )));

        let stats = ToolUsageStats::from_scrollback(&scrollback);
        let other = stats
            .categories
            .get(&ToolCategory::Other)
            .expect("other stats");
        assert_eq!(other.failed_count(), 0);
        assert_eq!(other.unconfirmed_count(), 1);
    }

    #[test]
    fn test_from_scrollback_mixed_categories() {
        let mut scrollback = ScrollbackState::new();

        // Push mixed tool blocks
        scrollback.push_block(RenderBlock::ToolCall(ToolCallBlock::Execute(
            ExecuteToolCallBlock::new("cargo build"),
        )));
        scrollback.push_block(RenderBlock::ToolCall(ToolCallBlock::Read(
            ReadToolCallBlock::new("src/main.rs"),
        )));
        scrollback.push_block(RenderBlock::ToolCall(ToolCallBlock::Edit(
            EditToolCallBlock::new("src/lib.rs", Vec::new()),
        )));
        scrollback.push_block(RenderBlock::ToolCall(ToolCallBlock::Execute(
            ExecuteToolCallBlock::new("cargo test"),
        )));

        let stats = ToolUsageStats::from_scrollback(&scrollback);

        assert_eq!(stats.total_operations, 4);
        assert_eq!(stats.scope, StatsScope::Session);

        // Verify category counts
        let execute_count = stats
            .categories
            .get(&ToolCategory::Execute)
            .map(|c| c.count)
            .unwrap_or(0);
        let read_count = stats
            .categories
            .get(&ToolCategory::Read)
            .map(|c| c.count)
            .unwrap_or(0);
        let edit_count = stats
            .categories
            .get(&ToolCategory::Edit)
            .map(|c| c.count)
            .unwrap_or(0);

        assert_eq!(execute_count, 2);
        assert_eq!(read_count, 1);
        assert_eq!(edit_count, 1);
    }

    #[test]
    fn test_from_scrollback_failed_blocks() {
        let mut scrollback = ScrollbackState::new();

        // Push execute blocks: one with error, one without
        let mut success_block = ExecuteToolCallBlock::new("echo hello");
        success_block.finish();
        scrollback.push_block(RenderBlock::ToolCall(ToolCallBlock::Execute(success_block)));

        let mut failed_block =
            ExecuteToolCallBlock::new("bad_command").with_error("command not found");
        failed_block.finish();
        scrollback.push_block(RenderBlock::ToolCall(ToolCallBlock::Execute(failed_block)));

        let stats = ToolUsageStats::from_scrollback(&scrollback);

        assert_eq!(stats.total_operations, 2);

        let exec_stats = stats.categories.get(&ToolCategory::Execute).unwrap();
        assert_eq!(exec_stats.count, 2);
        assert_eq!(exec_stats.failed_count(), 1);
    }

    #[test]
    fn test_from_selected_or_latest_empty_scrollback() {
        let scrollback = ScrollbackState::new();
        let stats = ToolUsageStats::from_selected_or_latest(&scrollback);

        assert_eq!(stats.total_operations, 0);
        assert_eq!(stats.scope, StatsScope::SelectedTurn);
    }

    #[test]
    fn test_from_selected_or_latest_no_turns_fallback() {
        let mut scrollback = ScrollbackState::new();

        // Push tool blocks without starting a turn
        scrollback.push_block(RenderBlock::ToolCall(ToolCallBlock::Execute(
            ExecuteToolCallBlock::new("echo test"),
        )));

        let stats = ToolUsageStats::from_selected_or_latest(&scrollback);

        // No turns exist, so it falls back to the latest turn (which is empty) or returns empty stats
        assert_eq!(stats.scope, StatsScope::SelectedTurn);
    }

    #[test]
    fn test_from_selected_or_latest_with_turns() {
        let mut scrollback = ScrollbackState::new();

        // Push user prompt (starts a turn) and some tool blocks
        scrollback.push_block(RenderBlock::user_prompt("test prompt"));
        scrollback.push_block(RenderBlock::ToolCall(ToolCallBlock::Execute(
            ExecuteToolCallBlock::new("echo hello"),
        )));
        scrollback.push_block(RenderBlock::ToolCall(ToolCallBlock::Read(
            ReadToolCallBlock::new("file.txt"),
        )));

        // Set as current turn
        scrollback.set_selected(Some(0));

        let stats = ToolUsageStats::from_selected_or_latest(&scrollback);

        assert_eq!(stats.scope, StatsScope::SelectedTurn);
        // Verify it computed without panicking
        let _ = stats.total_operations;
    }

    #[test]
    fn test_tool_block_is_failure() {
        let success = ExecuteToolCallBlock::new("echo test");
        assert!(!ToolUsageStats::tool_block_is_failure(
            &ToolCallBlock::Execute(success)
        ));

        let mut failed = ExecuteToolCallBlock::new("bad");
        failed.set_error(Some("error".into()));
        assert!(ToolUsageStats::tool_block_is_failure(
            &ToolCallBlock::Execute(failed)
        ));
    }
}
