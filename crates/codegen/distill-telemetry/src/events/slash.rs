//! Slash-command product telemetry events.

use serde::Serialize;

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum PagerCommandSource {
    Builtin,
    NonBuiltin,
}

#[derive(Serialize)]
pub struct SlashCommandUsed {
    pub command: String,
    pub args_provided: bool,
}

#[derive(Serialize)]
pub struct PagerSlashCommand {
    pub command_name: String,
    pub source: PagerCommandSource,
}
