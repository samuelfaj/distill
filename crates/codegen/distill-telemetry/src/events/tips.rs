//! Contextual-tip product telemetry events.

use serde::Serialize;

#[derive(Serialize, Clone, Copy, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ContextualTipKind {
    Undo,
    PlanMode,
    ImageInput,
    SendNow,
    SmallScreen,
    /// A double-click on the fold/nav path shows a tip to enable Word select in settings.
    WordSelect,
    /// Three nearby drag-copies → tip naming /copy and /export.
    ExportCopy,
    /// An SSH session without `grok wrap` shows a tip to wrap the ssh command locally.
    SshWrap,
}

#[derive(Serialize, Clone, Copy, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ContextualTipAction {
    Shown,
    Accepted,
}

/// One contextual-hint impression or acceptance: per tip, how often it is shown vs. acted on.
/// The `action` property drives the product-analytics funnel.
#[derive(Serialize)]
pub struct ContextualTip {
    pub tip: ContextualTipKind,
    pub action: ContextualTipAction,
}
