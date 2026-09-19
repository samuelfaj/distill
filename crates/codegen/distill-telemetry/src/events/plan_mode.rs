//! Plan-mode product telemetry events.

use serde::Serialize;

#[derive(Serialize, Clone, Copy, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum PlanModeTrigger {
    User,
    Tool,
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PlanModeState {
    Inactive,
    Pending,
    Active,
}

#[derive(Serialize)]
pub struct PlanModeToggled {
    pub enabled: bool,
    pub trigger: PlanModeTrigger,
    pub turn_in_flight: bool,
    pub was_previously_active: bool,
    /// Previous permission-mode label (`default` / `plan` / `bypass_permissions`)
    /// for the external `from_mode` attr. `#[serde(skip)]`.
    #[serde(skip)]
    pub from_mode: Option<String>,
}

#[derive(Serialize)]
pub struct PlanSubmit {
    pub action: String,
}
