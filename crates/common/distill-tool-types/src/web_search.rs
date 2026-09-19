//! Input and output types of the web search tool, shared with the crates that draw them

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The web search tool's arguments under the names the model sends them
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebSearchToolInput {
    #[schemars(
        description = "The search term to look up on the web. Be specific and include relevant keywords for better results. For technical queries, include version numbers or dates if relevant."
    )]
    pub search_term: String,

    #[schemars(
        description = "One sentence explanation as to why this tool is being used, and how it contributes to the goal."
    )]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

/// What a web search found, drawn as `content` lines with the `citations` counted as sites
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebSearchOutput {
    pub query: String,
    pub content: String,
    pub citations: Vec<String>,
    pub allowed_domains: Option<Vec<String>>,

    /// A complete model-visible body that replaces the default rendering of `content`
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pre_formatted: Option<String>,
}
