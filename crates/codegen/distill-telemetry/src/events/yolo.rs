//! Yolo-mode product telemetry events.

use serde::Serialize;

#[derive(Serialize, Clone, Copy, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum YoloTrigger {
    SlashCommand,
    ClientMeta,
    Pager,
}

#[derive(Serialize)]
pub struct YoloToggled {
    pub enabled: bool,
    pub previous_state: bool,
    pub trigger: YoloTrigger,
    /// Previous permission mode (`default` / `plan` / `bypass_permissions`).
    /// `None` falls back to yolo-only derivation from `previous_state`.
    #[serde(skip)]
    pub from_mode: Option<String>,
}
