// Modified for Distill by Samuel Fajreldines, 2026.
//! `Tool::should_list` predicate + `ToolDyn` blanket forwarding.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use distill_tool_protocol::ToolId;
use distill_tool_runtime::{
    ArcTool, Cwd, ListToolsContext, Tool, ToolCallContext, ToolError, ToolOutput,
};
use distill_tool_types::ToolDescription;

#[derive(Deserialize, JsonSchema)]
struct NoArgs {}

#[derive(Serialize)]
struct Unit;

impl ToolOutput for Unit {}

struct AlwaysTool;

impl Tool for AlwaysTool {
    type Args = NoArgs;
    type Output = Unit;
    fn id(&self) -> ToolId {
        ToolId::new("always").unwrap()
    }
    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new("always", "a")
    }
    async fn run(&self, _: ToolCallContext, _: NoArgs) -> Result<Unit, ToolError> {
        Ok(Unit)
    }
}

struct NeedsCwdTool;

impl Tool for NeedsCwdTool {
    type Args = NoArgs;
    type Output = Unit;
    fn id(&self) -> ToolId {
        ToolId::new("needs_cwd").unwrap()
    }
    fn description(&self, _ctx: &ListToolsContext) -> ToolDescription {
        ToolDescription::new("needs_cwd", "a")
    }
    fn should_list(&self, ctx: &ListToolsContext) -> bool {
        ctx.extensions.contains::<Cwd>()
    }
    async fn run(&self, _: ToolCallContext, _: NoArgs) -> Result<Unit, ToolError> {
        Ok(Unit)
    }
}

#[test]
fn default_returns_true() {
    assert!(Tool::should_list(&AlwaysTool, &ListToolsContext::default()));
}

#[test]
fn dyn_forwards_custom() {
    let tool: ArcTool = Arc::new(NeedsCwdTool);
    assert!(!tool.should_list(&ListToolsContext::default()));

    let mut ctx = ListToolsContext::default();
    ctx.extensions
        .insert(Cwd(std::path::PathBuf::from("/home")));
    assert!(tool.should_list(&ctx));
}
