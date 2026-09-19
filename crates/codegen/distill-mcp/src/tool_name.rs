// Modified for Distill by Samuel Fajreldines, 2026.
//! MCP tool-name qualification and session admission.
//!
//! Provider function-name limits (64 chars) apply to `search_tool` / `use_tool`, not to
//! catalog keys. A qualified `server__tool` is a routing id for those meta-tools.

use distill_workspace_types::MCP_TOOL_NAME_DELIMITER;

/// Strictest cross-provider function-name length (`search_tool`, `use_tool`, …).
pub const PROVIDER_TOOL_NAME_MAX_CHARS: usize = 64;

/// Safety cap on catalog keys (`server__tool`). Not a provider function-name limit.
pub const MCP_QUALIFIED_NAME_MAX_CHARS: usize = 256;

/// Why a listed MCP tool was not admitted into the session catalog.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpToolAdmissionError {
    #[error(
        "server name '{name}' is invalid — must start with a letter or underscore and contain only letters, digits, underscores, and hyphens"
    )]
    InvalidServerName { name: String },
    #[error(
        "tool name '{name}' is invalid — must contain only letters, digits, underscores, and hyphens"
    )]
    InvalidToolName { name: String },
    #[error("qualified MCP name '{qualified}' is {len} chars; max is {max}")]
    QualifiedNameTooLong {
        qualified: String,
        len: usize,
        max: usize,
    },
    #[error("MCP tool has invalid or ambiguous qualified name '{qualified}'")]
    InvalidOrAmbiguousQualifiedName { qualified: String },
}

/// Validate that a tool name matches the strictest cross-provider LLM API requirements.
pub fn validate_tool_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("tool name cannot be empty".to_owned());
    }
    if name.len() > PROVIDER_TOOL_NAME_MAX_CHARS
        || !starts_with_letter_or_underscore(name)
        || !is_mcp_name_segment(name)
    {
        return Err(format!(
            "tool name '{name}' is invalid — must match ^[a-zA-Z_][a-zA-Z0-9_-]{{0,63}}$ (start with letter/underscore, max {PROVIDER_TOOL_NAME_MAX_CHARS} chars)"
        ));
    }
    Ok(())
}

/// Parse a non-empty `server__tool` ID with one overlap-aware delimiter and valid [`distill_tool_protocol::ToolId`] syntax.
pub fn parse_mcp_qualified_name(name: &str) -> Option<(distill_tool_protocol::ToolId, &str, &str)> {
    let delimiter = MCP_TOOL_NAME_DELIMITER.as_bytes();
    // Byte windows preserve both overlapping `__` boundaries in `___`.
    let mut boundaries = name
        .as_bytes()
        .windows(delimiter.len())
        .enumerate()
        .filter_map(|(index, window)| (window == delimiter).then_some(index));
    let boundary = boundaries.next()?;
    if boundaries.next().is_some() {
        return None;
    }
    let (server, tool_with_delimiter) = name.split_at(boundary);
    let tool = tool_with_delimiter.get(MCP_TOOL_NAME_DELIMITER.len()..)?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((distill_tool_protocol::ToolId::new(name).ok()?, server, tool))
}

/// Parse an MCP tool name in `server__tool` format into owned segments.
pub fn parse_mcp_tool_name(name: &str) -> Option<(String, String)> {
    parse_mcp_qualified_name(name).map(|(_, server, tool)| (server.to_owned(), tool.to_owned()))
}

/// Qualify `server` + raw MCP `tool` for the session catalog.
///
/// Segments are nonempty `[A-Za-z0-9_-]+` ([`distill_tool_protocol::ToolId`] charset).
/// The server prefixes the catalog key and must start with a letter or underscore.
/// The raw tool segment may start with a digit (`auth__2fa_enable`).
/// The concatenated key may exceed the 64-char provider function-name limit.
pub fn qualify_mcp_tool_name(server: &str, tool: &str) -> Result<String, McpToolAdmissionError> {
    if !starts_with_letter_or_underscore(server) || !is_mcp_name_segment(server) {
        return Err(McpToolAdmissionError::InvalidServerName {
            name: server.to_owned(),
        });
    }
    if !is_mcp_name_segment(tool) {
        return Err(McpToolAdmissionError::InvalidToolName {
            name: tool.to_owned(),
        });
    }
    let qualified = format!("{server}{MCP_TOOL_NAME_DELIMITER}{tool}");
    if qualified.len() > MCP_QUALIFIED_NAME_MAX_CHARS {
        return Err(McpToolAdmissionError::QualifiedNameTooLong {
            len: qualified.len(),
            qualified,
            max: MCP_QUALIFIED_NAME_MAX_CHARS,
        });
    }
    if parse_mcp_qualified_name(&qualified).is_none() {
        return Err(McpToolAdmissionError::InvalidOrAmbiguousQualifiedName { qualified });
    }
    Ok(qualified)
}

fn is_mcp_name_segment(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn starts_with_letter_or_underscore(name: &str) -> bool {
    matches!(name.chars().next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
}

#[cfg(test)]
#[path = "tool_name_tests.rs"]
mod tests;
