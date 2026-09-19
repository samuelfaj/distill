//! Helpers shared by the dashboard render and chrome test modules.

use ratatui::buffer::Buffer;

use super::row::DashboardRow;
use super::state::{DashboardRowId, RowState};

/// Visible text in left-to-right order, including wide-cell glyphs.
pub(super) fn buf_to_text(buf: &Buffer) -> String {
    let mut content = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(cell) = buf.cell((x, y)) {
                content.push_str(cell.symbol());
            }
        }
        content.push('\n');
    }
    content
}

/// A top-level dashboard row with defaults, for chrome and render tests.
pub(super) fn header_test_row(id: u32, state: RowState, label: &str) -> DashboardRow {
    use crate::app::agent::AgentId;
    DashboardRow {
        id: DashboardRowId::TopLevel(AgentId(id as usize)),
        session_id: None,
        label: label.to_string(),
        subtitle: None,
        state,
        activity: None,
        secondary_line: None,
        cwd_display: String::new(),
        cwd: std::path::PathBuf::from("/tmp"),
        last_change_at: std::time::SystemTime::now(),
        pinned: false,
        badges: Vec::new(),
        context_pct: None,
        indent: 0,
        parent_label: None,
        is_more_placeholder: false,
        more_count: 0,
    }
}
