//! Extensions-modal product telemetry events.

use serde::Serialize;

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionsModalTrigger {
    SlashCommand,
    KeyboardShortcut,
    CommandPalette,
    AuthHandoff,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionsInputMethod {
    Keyboard,
    Mouse,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionsModalTab {
    Hooks,
    Plugins,
    Marketplace,
    Skills,
    Workflows,
    McpServers,
}

#[derive(Serialize)]
pub struct ExtensionsModalOpened {
    pub trigger: ExtensionsModalTrigger,
    pub tab: ExtensionsModalTab,
}

#[derive(Serialize)]
pub struct ExtensionsModalAction {
    pub tab: ExtensionsModalTab,
    pub action: String,
    pub input_method: ExtensionsInputMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}
