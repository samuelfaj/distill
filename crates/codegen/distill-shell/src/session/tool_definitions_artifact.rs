// Modified for Distill by Samuel Fajreldines, 2026.
//! `{session_dir}/tool_definitions.json`: the function tools a session sent the model on its latest sampling iteration.
//!
//! The file is the Chat-Completions-shaped array the wire carries (`{ "type": "function", "function": { … } }`): value-equal
//! to the request's `tools[]` and byte-identical to the uploaded trace copy. Hosted backend-search tools are not `ToolSpec`s
//! and never appear; an empty toolset is written as `[]` even though the wire omits the key. An unchanged toolset costs no
//! directory resolution or I/O, and a reader never observes a torn file (`write_bytes_atomic`).

use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::Path;

use distill_tools::types::template_renderer::unresolved_template_markers;

use crate::sampling::ToolSpec;
use crate::sampling::types::ToolDefinition;
use crate::session::acp_session::SessionActor;
use crate::session::info::Info;

pub const TOOL_DEFINITIONS_FILENAME: &str = "tool_definitions.json";

/// Content hash of the artifact last written for a session, kept in the `ToolBridge` resources.
/// Absent until the first successful write; an agent rebuild starts from a fresh resource map, so its first iteration rewrites.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ToolDefinitionsArtifactHash(u64);

/// Equal hashes mean the file on disk needs no rewrite.
fn tool_definitions_hash(specs: &[ToolSpec]) -> u64 {
    let mut hasher = DefaultHasher::new();
    specs.len().hash(&mut hasher);
    for spec in specs {
        spec.name.hash(&mut hasher);
        spec.description.hash(&mut hasher);
        spec.parameters.hash(&mut hasher);
    }
    hasher.finish()
}

/// The artifact body: `definitions` pretty-printed.
///
/// # Errors
///
/// A serialization failure surfaces as `InvalidData`.
fn tool_definitions_bytes(definitions: &[ToolDefinition]) -> io::Result<Vec<u8>> {
    serde_json::to_vec_pretty(definitions)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes an artifact body into `dir` atomically.
///
/// # Errors
///
/// Any `io::Error` from `write_bytes_atomic`.
fn write_tool_definitions_bytes(dir: &Path, bytes: &[u8]) -> io::Result<()> {
    crate::session::storage::write_bytes_atomic(&dir.join(TOOL_DEFINITIONS_FILENAME), bytes)
}

/// Creates the session dir and writes the artifact on a blocking thread: the durable mkdir and the fsync'd write must
/// not stall the sampling loop on the actor's `LocalSet`.
///
/// # Errors
///
/// Any `io::Error` from creating the directory or writing the file; a blocking-task join failure surfaces as `Other`.
async fn write_tool_definitions_artifact(info: Info, bytes: Vec<u8>) -> io::Result<()> {
    tokio::task::spawn_blocking(move || {
        let dir = crate::session::persistence::ensure_owner_only_session_dir(&info)?;
        write_tool_definitions_bytes(&dir, &bytes)
    })
    .await
    .map_err(io::Error::other)?
}

// TODO: surface this through a `grok inspect --tools` reader.
/// Reads the artifact back.
///
/// # Errors
///
/// `NotFound` when the session has not sampled yet, any other read error as-is, and `InvalidData` for a malformed file.
pub fn load_tool_definitions_from_dir(dir: &Path) -> io::Result<Vec<ToolDefinition>> {
    let json = std::fs::read(dir.join(TOOL_DEFINITIONS_FILENAME))?;
    serde_json::from_slice(&json).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

impl SessionActor {
    /// Hash first, so an unchanged toolset costs no directory resolution or I/O; every failure is a warning and the turn proceeds.
    /// The hash is recorded only after a successful write, so a failed attempt is retried on the next sampling iteration.
    pub(crate) async fn persist_tool_definitions_artifact(&self, specs: &[ToolSpec]) {
        let hash = ToolDefinitionsArtifactHash(tool_definitions_hash(specs));
        let bridge = self.agent.borrow().tool_bridge().clone();
        if bridge.read_resource::<ToolDefinitionsArtifactHash>().await == Some(hash) {
            return;
        }
        let definitions: Vec<ToolDefinition> = specs
            .iter()
            .map(|spec| {
                ToolDefinition::function(
                    spec.name.as_str(),
                    spec.description.as_deref(),
                    spec.parameters.clone(),
                )
            })
            .collect();
        let bytes = match tool_definitions_bytes(&definitions) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(session_id = %self.session_info.id.0, ?e,
                    "failed to serialize tool_definitions.json");
                return;
            }
        };
        if let Err(e) = write_tool_definitions_artifact(self.session_info.clone(), bytes).await {
            tracing::warn!(session_id = %self.session_info.id.0, ?e,
                "failed to write tool_definitions.json");
            return;
        }
        bridge.update_resource(hash).await;
        let offenders = unresolved_template_markers(&definitions);
        if !offenders.is_empty() {
            tracing::warn!(session_id = %self.session_info.id.0, tools = ?offenders,
                "tool definitions contain unresolved template markers");
        }
    }
}

#[cfg(test)]
#[path = "tool_definitions_artifact_tests.rs"]
mod tests;
