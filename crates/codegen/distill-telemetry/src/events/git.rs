//! Git, PR, and multi-agent product telemetry events.

use crate::enums::PrCreationSource;
use serde::Serialize;

/// PR created via the session (bash `gh pr create` or MCP create_pull_request).
/// Counts only: PR url/number stay in the turn_result.json signals.
#[derive(Serialize)]
pub struct PrCreated {
    pub source: PrCreationSource,
    /// Whether the session recorded a `git commit` before the create; separates work done end to end in the session from work started elsewhere.
    pub had_commit_in_session: bool,
}

/// PR merged via the session bash tool (`gh pr merge`).
#[derive(Serialize)]
pub struct PrMerged {}

#[derive(Serialize)]
pub struct MultiAgentFollowup {
    pub preferred_agent_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_agent_model_id: Option<String>,
    pub other_agents: Vec<AgentInfo>,
    pub total_agents: usize,
}

#[derive(Serialize)]
pub struct MultiAgentApply {
    pub applied_agent_label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_agent_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_agent_model_id: Option<String>,
    pub discarded_agents: Vec<AgentInfo>,
    pub total_agents: usize,
}

#[derive(Serialize)]
pub struct MultiAgentDiscard {
    pub discarded_agents: Vec<AgentInfo>,
    pub total_agents_discarded: usize,
}

#[derive(Serialize)]
pub struct AgentInfo {
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

#[derive(Serialize)]
pub struct RepoChanges {
    pub commit_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_files_changed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_insertions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged_deletions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unstaged_files_changed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unstaged_insertions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unstaged_deletions: Option<u64>,
    pub untracked_file_count: usize,
    pub untracked_total_bytes: u64,
    pub is_detached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection_id: Option<String>,
}

#[derive(Serialize)]
pub struct NonGitDecisionEvent {
    pub decision: String,
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}
