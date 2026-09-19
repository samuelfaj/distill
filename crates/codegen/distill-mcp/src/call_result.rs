// Modified for Distill by Samuel Fajreldines, 2026.
//! Model-visible text for an MCP `tools/call` result.

use distill_tools::types::output::MCPOutput;
use distill_tools::util::mcp_structured_content::render_structured_content;

/// Error results keep only their text blocks; both branches get [`render_structured_content`].
pub(crate) fn mcp_output_from_call_result(
    tool: String,
    server: String,
    call_result: rmcp::model::CallToolResult,
    expose_base64: bool,
) -> MCPOutput {
    let is_error = call_result.is_error.unwrap_or(false);
    let mut parts: Vec<String> = call_result
        .content
        .into_iter()
        .filter_map(|c| match c {
            rmcp::model::ContentBlock::Text(t) => Some(t.text),
            _ if is_error => None,
            rmcp::model::ContentBlock::Image(img) => {
                Some(format_mcp_image(&img.mime_type, &img.data, expose_base64))
            }
            rmcp::model::ContentBlock::Resource(r) => match &r.resource {
                rmcp::model::ResourceContents::BlobResourceContents {
                    mime_type: Some(mime),
                    blob,
                    ..
                } if mime.starts_with("image/") => {
                    Some(format_mcp_image(mime, blob, expose_base64))
                }
                _ => serde_json::to_string(&r).ok(),
            },
            _ => None,
        })
        .collect();
    parts.extend(render_structured_content(
        call_result.structured_content.as_ref(),
        parts.iter().map(String::as_str),
    ));

    let text = parts.join("\n");
    if is_error {
        MCPOutput::errored(tool, server, text)
    } else {
        MCPOutput::okay_output(tool, server, text)
    }
}

/// The wrapper has no `data:image/` prefix, so the image-extraction regex skips it.
fn format_mcp_image(mime: &str, base64_data: &str, expose_base64: bool) -> String {
    if expose_base64 {
        format!(
            "data:{mime};base64,{base64_data}\n\
             <mcp_image_base64 mime=\"{mime}\">\n\
             {base64_data}\n\
             </mcp_image_base64>"
        )
    } else {
        format!("data:{mime};base64,{base64_data}")
    }
}

#[cfg(test)]
#[path = "call_result_tests.rs"]
mod tests;
