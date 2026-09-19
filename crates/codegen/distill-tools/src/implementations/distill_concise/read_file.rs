// Modified for Distill by Samuel Fajreldines, 2026.
//! Concise variant of the `read_file` tool.

use crate::implementations::distill::read_file::{ReadFileInput, run_read_file};

const DESCRIPTION_CONCISE: &str = r#"Reads a file from the computer's filesystem. You can access any file directly by using this tool.
It is okay to read a file that does not exist; an error will be returned.

Usage:
- You can optionally specify ${{ params.read.offset }} and ${{ params.read.limit }} (especially handy for long files).
- Lines in the output are numbered starting at 1, using following format: LINE_NUMBER→LINE_CONTENT.
- You have the capability to call multiple tools in a single response. It is always better to speculatively read multiple files as a batch that are potentially useful."#;
use crate::types::output::ReadFileOutput;
use crate::types::requirements::{Expr, ToolRequirement};
use crate::types::tool::{ToolKind, ToolNamespace};

/// Concise variant of `ReadFileTool`. Delegates to `run_read_file()`, then swaps `content_concise`
/// into `content` (no line-number padding).
#[derive(Debug, Default)]
pub struct ReadFileConciseTool;

impl crate::types::tool_metadata::ToolMetadata for ReadFileConciseTool {
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }

    fn tool_namespace(&self) -> ToolNamespace {
        ToolNamespace::DistillConcise
    }

    fn description_template(&self) -> &str {
        DESCRIPTION_CONCISE
    }

    fn requires_expr(&self) -> Expr<ToolRequirement> {
        Expr::True
    }
}

impl distill_tool_runtime::Tool for ReadFileConciseTool {
    type Args = ReadFileInput;
    type Output = ReadFileOutput;

    fn id(&self) -> distill_tool_protocol::ToolId {
        distill_tool_protocol::ToolId::new("read_file").expect("valid tool id")
    }

    fn description(
        &self,
        _ctx: &::distill_tool_runtime::ListToolsContext,
    ) -> distill_tool_types::ToolDescription {
        distill_tool_types::ToolDescription::new(
            "read_file",
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

    #[tracing::instrument(
        name = "tool.read_file_concise",
        skip_all,
        fields(path = %input.path)
    )]
    async fn run(
        &self,
        ctx: distill_tool_runtime::ToolCallContext,
        input: ReadFileInput,
    ) -> Result<ReadFileOutput, distill_tool_runtime::ToolError> {
        use crate::types::tool_metadata::shared_resources;
        let resources = shared_resources(&ctx)?;

        // DistillConcise is not version-managed — always pass None.
        let cwd_override = ctx
            .extensions
            .get::<distill_tool_runtime::Cwd>()
            .map(|c| c.0.clone());
        // `None`: the concise tool does not stream, so it needs no
        // text-path streamability signal (see `run_read_file`).
        let invoking = crate::types::tool_metadata::invoking_param_names(&ctx);
        let result = run_read_file(input, cwd_override, None, resources, None, &invoking).await?;

        match result {
            ReadFileOutput::FileContent(mut fc) => {
                if let Some(concise) = fc.content_concise.take() {
                    fc.content = concise;
                }
                Ok(ReadFileOutput::FileContent(fc))
            }
            other => Ok(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tool_metadata::test_ctx;

    use crate::computer::local::LocalFs;
    use crate::implementations::distill::read_file::ReadFileTool;
    use crate::notification::types::ToolNotificationHandle;
    use crate::types::resources::{Cwd, FileSystem, NotificationHandle, Resources};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_resources(cwd: &std::path::Path) -> Resources {
        let mut resources = Resources::new();
        resources.insert(Cwd(cwd.to_path_buf()));
        resources.insert(FileSystem(Arc::new(LocalFs)));
        resources.insert(NotificationHandle(ToolNotificationHandle::noop()));
        resources
    }

    #[test]
    fn description_template_selects_concise() {
        use crate::types::tool_metadata::ToolMetadata;
        let default_tool = ReadFileTool;
        let concise_tool = ReadFileConciseTool;
        assert_ne!(
            ToolMetadata::description_template(&default_tool),
            ToolMetadata::description_template(&concise_tool),
        );
    }

    #[test]
    fn description_template_tracks_renamed_offset_limit() {
        use crate::types::template_renderer::TemplateRenderer;
        use crate::types::tool_metadata::ToolMetadata;
        use std::collections::HashMap;

        let tools = HashMap::from([(ToolKind::Read, "read_file".to_string())]);
        let params = HashMap::from([(
            ToolKind::Read,
            HashMap::from([
                ("offset".to_string(), "start_line".to_string()),
                ("limit".to_string(), "num_lines".to_string()),
            ]),
        )]);
        let rendered = TemplateRenderer::new(tools, params)
            .render(ToolMetadata::description_template(&ReadFileConciseTool))
            .unwrap();
        assert!(
            rendered.contains("start_line") && rendered.contains("num_lines"),
            "renamed offset/limit must appear:\n{rendered}"
        );
    }

    #[tokio::test]
    async fn concise_mode_uses_concise_content() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "hello\nworld\n").unwrap();

        let tool = ReadFileConciseTool;
        let resources = test_resources(tmp.path());

        let input = ReadFileInput {
            path: "test.txt".to_string(),
            offset: None,
            limit: None,
            pages: None,
            format: None,
        };

        let result = distill_tool_runtime::Tool::run(&tool, test_ctx(resources.into_shared()), input)
            .await
            .unwrap();
        match result {
            ReadFileOutput::FileContent(content) => {
                assert_eq!(content.content, "1→hello\nworld\n");
            }
            other => panic!("Expected FileContent, got {:?}", other),
        }
    }
}
