// Modified for Distill by Samuel Fajreldines, 2026.
use std::path::PathBuf;

use crate::resume::ResumeValidationError;

/// How the child session's initial context was bootstrapped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextSource {
    /// Fresh session with no inherited history.
    New,
    /// Resumed from a previously completed peer subagent.
    /// The child inherits the source's raw transcript, tool state, and model.
    /// System prompt and prompt context are freshly rendered.
    Resumed,
}

/// Resolved effective runtime configuration for a child agent.
///
/// Precedence: explicit spawn-time override > role default > persona default > parent inheritance (None).
#[derive(Debug, Clone, Default)]
pub struct EffectiveRuntimeConfig {
    /// Resolved model ID override (if any).
    pub model: Option<String>,
    /// Resolved reasoning effort (e.g. "low", "medium", "high").
    // TODO: consider a typed `ReasoningEffort` enum to prevent typos
    // It stays a plain string for compatibility with the shell's existing API
    pub reasoning_effort: Option<String>,
    /// Resolved capability mode controlling tool access.
    pub capability_mode: Option<distill_tool_types::SubagentCapabilityMode>,
    /// Resolved persona name (for metadata/observability).
    pub persona: Option<String>,
    /// Resolved persona instructions text (for prompt assembly).
    pub persona_instructions: Option<String>,
    /// Role prompt_file content (loaded at resolve time).
    pub role_prompt: Option<String>,
    /// Warning when role prompt_file failed to load (soft degradation).
    pub role_prompt_warning: Option<String>,
    /// Resolved role name (the key that matched in subagent_roles lookup).
    pub role_name: Option<String>,
    /// Error from persona resolution (file unreadable, not found, empty).
    /// Unlike role prompts, persona errors are fatal: spawn is aborted.
    pub persona_error: Option<String>,
    /// Isolation mode for the child execution environment.
    pub isolation: distill_tool_types::SubagentIsolationMode,
}

/// Data about a completed source subagent, needed to validate a resume and to spawn the resumed child.
#[derive(Debug, Clone)]
pub struct ResumeSourceData {
    /// Source subagent ID.
    pub subagent_id: String,
    /// Source subagent type (e.g. "general-purpose", "explore").
    /// Used by `validate_resume_identity` to check type match.
    pub subagent_type: String,
    /// Source subagent persona, if any.
    /// Used by `validate_resume_identity` to check persona match.
    pub persona: Option<String>,
    /// Effective model ID used by the source child session.
    /// The shell uses this model when no explicit model override wins on resume.
    pub model_id: Option<String>,
    /// Parent Jev effort-routing policy captured with the source session.
    /// Missing on legacy metadata, where callers use the conservative manual default.
    pub effort_auto: Option<bool>,
    /// Whether the source's model was explicitly pinned for Jev routing.
    /// This is separate from `effort_auto`: `model=A, reasoning_effort=auto`
    /// keeps A fixed while still allowing per-round effort selection.
    /// Missing on legacy metadata, where the host uses its conservative policy.
    pub model_routing_locked: Option<bool>,
    /// Effective cwd the source child used.
    /// The shell uses it to reconstruct `SessionInfo` so the raw transcript continues and the worktree can be reused.
    pub child_cwd: String,
    /// Worktree path if the source used `isolation=worktree`.
    /// The shell reuses this directory when resuming a worktree-isolated child.
    pub worktree_path: Option<PathBuf>,
    /// Durable git ref holding a snapshot of the source worktree's working state, set when the worktree was snapshotted at completion.
    /// The shell uses it to recreate a deleted worktree directory on resume.
    pub snapshot_ref: Option<String>,
    /// The child session ID of the source subagent.
    /// The shell uses it to locate the source's session directory and copy the raw transcript (`copy_session_data_sync`).
    pub child_session_id: String,
}

impl From<distill_tools::implementations::distill::task::types::SubagentResumeSource>
    for ResumeSourceData
{
    fn from(
        source: distill_tools::implementations::distill::task::types::SubagentResumeSource,
    ) -> Self {
        ResumeSourceData {
            subagent_id: source.subagent_id,
            subagent_type: source.subagent_type,
            persona: source.persona,
            model_id: source.model_id,
            effort_auto: None,
            model_routing_locked: None,
            child_cwd: source.child_cwd,
            worktree_path: source.worktree_path.map(PathBuf::from),
            snapshot_ref: source.snapshot_ref,
            child_session_id: source.child_session_id,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolutionError {
    /// No production or session CLI definition has this name.
    #[error("unknown subagent type \"{subagent_type}\"; available: {available:?}")]
    Unknown {
        subagent_type: String,
        available: Vec<String>,
    },

    /// The definition exists but is disabled by the session toggle.
    #[error("subagent \"{subagent_type}\" is disabled")]
    Disabled { subagent_type: String },

    /// The parent session restricts which child types may run.
    #[error("subagent \"{subagent_type}\" is not allowed; allowed: {allowed:?}")]
    NotAllowed {
        subagent_type: String,
        allowed: Vec<String>,
    },

    /// Persona was explicitly requested but could not be resolved.
    #[error("persona resolution failed: {0}")]
    PersonaResolution(String),

    /// Resume identity validation failed.
    #[error("resume validation failed: {0}")]
    ResumeValidation(#[from] ResumeValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_error_persona_display() {
        let err = ResolutionError::PersonaResolution("persona \"x\" not found".into());
        assert_eq!(
            err.to_string(),
            "persona resolution failed: persona \"x\" not found",
        );
    }

    #[test]
    fn resolution_error_resume_from_typed_error() {
        let typed = ResumeValidationError::TypeMismatch {
            requested: "explore".into(),
            source_value: "general-purpose".into(),
        };
        let err = ResolutionError::from(typed);
        let msg = err.to_string();
        assert!(msg.contains("resume validation failed"));
        assert!(msg.contains("explore"));
        assert!(msg.contains("general-purpose"));
    }
}
