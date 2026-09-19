//! Plugin lifecycle and CTA product telemetry events.

use serde::Serialize;

#[derive(Serialize, Clone, Copy, strum::AsRefStr, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum InstallKind {
    Git,
    Local,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PluginSource {
    LocalPath,
    Git,
}

#[derive(Serialize)]
pub struct PluginAdded {
    pub source: PluginSource,
    pub success: bool,
}

#[derive(Serialize)]
pub struct PluginRemoved {
    pub success: bool,
}

#[derive(Serialize)]
pub struct PluginInstalled {
    pub install_kind: InstallKind,
    pub trust: bool,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
}

#[derive(Serialize)]
pub struct PluginUninstalled {
    pub confirmed: bool,
    pub success: bool,
}

#[derive(Serialize)]
pub struct PluginReloaded {
    pub success: bool,
}

#[derive(Serialize)]
pub struct PluginUsed {
    pub plugin_id: String,
    pub plugin_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_event: Option<String>,
    pub success: bool,
}

#[derive(Serialize)]
pub struct PluginCtaImpression {
    pub plugin_name: String,
}

#[derive(Serialize)]
pub struct PluginCtaConnectClicked {
    pub plugin_name: String,
    pub is_retry: bool,
}

#[derive(Serialize)]
pub struct PluginCtaDismissed {
    pub plugin_name: String,
}

#[derive(Serialize)]
pub struct PluginCtaInstalled {
    pub plugin_name: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_category: Option<String>,
}
