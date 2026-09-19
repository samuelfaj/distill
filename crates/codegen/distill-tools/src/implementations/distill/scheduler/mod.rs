pub mod actor;
pub mod create;
pub mod delete;
pub mod interval;
pub mod list;
pub(crate) mod occurrence_journal;
pub mod types;

use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::ToolKind;

pub(crate) fn scheduler_bundle_requires_expr() -> Expr<ToolRequirement> {
    Expr::And(vec![
        Expr::Value(ToolRequirement::tool::<create::SchedulerCreateTool>()),
        Expr::Value(ToolRequirement::tool::<delete::SchedulerDeleteTool>()),
        Expr::Value(ToolRequirement::tool::<list::SchedulerListTool>()),
        Expr::Value(ToolRequirement::tool_kind(ToolKind::BackgroundTaskAction)),
    ])
}
