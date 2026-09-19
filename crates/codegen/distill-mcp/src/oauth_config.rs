//! The host's TOML parsing (`McpServerConfig::oauth_config`) builds these types; [`crate::oauth`] consumes them.

use std::collections::HashMap;

/// Travels alongside `acp::McpServer` (which can't be extended since it's an external crate type).
#[derive(Debug, Clone, Default)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub callback_port: Option<u16>,
}

impl McpOAuthConfig {
    pub fn is_configured(&self) -> bool {
        self.client_id.is_some()
    }
}

/// Per-server OAuth configuration map, keyed by MCP server name.
pub type McpOAuthConfigMap = HashMap<String, McpOAuthConfig>;
