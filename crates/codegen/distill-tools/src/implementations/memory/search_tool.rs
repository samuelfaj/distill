// Modified for Distill by Samuel Fajreldines, 2026.
//! `memory_search` tool — new architecture (`Tool` trait).

use std::sync::Arc;

use super::types::MemorySearchInput;
use crate::types::memory_backend::{MemoryBackend, format_staleness_note};
use crate::types::output::ToolOutput;
use crate::types::tool::{ToolKind, ToolNamespace};

#[derive(Debug, Default)]
pub struct MemorySearchImpl;

impl crate::types::tool_metadata::ToolMetadata for MemorySearchImpl {
    fn kind(&self) -> ToolKind {
        ToolKind::MemorySearch
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::Distill
    }

    fn description_template(&self) -> &str {
        "Search cross-session memory for relevant knowledge chunks. Returns ranked results \
         from global, workspace, and session memory files.\n\n\
         Use this proactively when:\n\
         - A question references prior work, decisions, or context you don't have\n\
         - You need project conventions, coding patterns, or user preferences\n\
         - The user mentions something discussed or decided in a previous session\n\
         - Starting work in an unfamiliar part of the codebase\n\
         - After compaction when prior context may have been lost\n\n\
         Memory is historical context, not automatically the current plan. Verify recalled facts \
         against live sources before relying on them."
    }
}

impl distill_tool_runtime::Tool for MemorySearchImpl {
    type Args = MemorySearchInput;
    type Output = ToolOutput;

    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new("memory_search").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(
            "memory_search",
            crate::types::tool_metadata::ToolMetadata::sanitized_description_template(self),
        )
    }

    fn capabilities(&self) -> distill_tool_protocol::ToolCapabilities {
        distill_tool_protocol::ToolCapabilities {
            is_read_only: true,
            tool_scope: Some(distill_tool_protocol::ToolScope::Read),
            ..Default::default()
        }
    }

    async fn run(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        input: MemorySearchInput,
    ) -> Result<ToolOutput, distill_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;
        let Some(memory) = resources
            .lock()
            .await
            .get::<Arc<dyn MemoryBackend>>()
            .cloned()
        else {
            return Ok(ToolOutput::Text(
                "Memory is not enabled. Use --experimental-memory to enable.".into(),
            ));
        };
        let max_results = input
            .max_results
            .unwrap_or_else(|| memory.default_search_max_results());
        let min_score = input
            .min_score
            .unwrap_or_else(|| memory.default_search_min_score());
        tracing::info!(target: crate::types::memory_backend::MEMORY_LOG_TARGET, max_results, "MEMORY_SEARCH: invoked");
        let results = memory
            .search(&input.query, max_results, min_score)
            .await
            .map_err(|e| {
                distill_tool_runtime::ToolError::execution(
                    distill_tool_protocol::ToolId::new("memory_search").expect("valid"),
                    format!("memory search failed: {e}"),
                )
            })?;
        tracing::info!(target: crate::types::memory_backend::MEMORY_LOG_TARGET, results = results.len(), "MEMORY_SEARCH: complete");
        if results.is_empty() {
            return Ok(ToolOutput::Text(
                "No memory results found for query.".into(),
            ));
        }
        let mut output = format!("Found {} memory result(s):\n", results.len());
        for (i, r) in results.iter().enumerate() {
            let staleness = format_staleness_note(&r.source, r.created_at);
            output.push_str(&format!(
                "\n### Result {} (score: {:.2}, source: {})\n**File:** {} (lines {}-{})\n{}```\n{}\n```\n",
                i + 1, r.score, r.source, r.path, r.start_line, r.end_line, staleness, r.snippet,
            ));
        }
        Ok(ToolOutput::Text(output.into()))
    }
}
