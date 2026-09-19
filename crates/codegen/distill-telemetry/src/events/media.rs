//! Media-generation product telemetry events.

use serde::Serialize;

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum MediaType {
    Image,
    Video,
}

#[derive(Serialize)]
pub struct MediaGenerated {
    pub media_type: MediaType,
    pub success: bool,
    pub prompt_length: usize,
}
