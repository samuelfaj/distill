// Modified for Distill by Samuel Fajreldines, 2026.
//! The agent surface that extension handlers operate against.
//!
//! Extension modules name this trait instead of the concrete run-loop agent, so
//! the run loop can implement it while the extensions stay independent of it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use distill_login::AuthManager;

use crate::agent::session_registry_client::SessionRegistryClient;
use crate::extensions::code_nav::CodeNavEligibility;
use crate::session::SessionHandle;
use crate::session::worktree::BackgroundCopyContext;
use crate::util::config::{RemoteSettings, WorktreeType};

/// Host services an extension handler needs from the agent that runs it.
#[async_trait::async_trait(?Send)]
pub trait AgentRuntime {
    fn gateway(&self) -> &GatewaySender;

    fn remote_settings(&self) -> Option<RemoteSettings>;

    fn auth_manager(&self) -> &Arc<AuthManager>;

    fn worktree_type(&self) -> WorktreeType;

    fn restore_code(&self) -> bool;

    fn get_session_cwd(&self, session_id: &acp::SessionId) -> Option<PathBuf>;

    fn refresh_skill_baseline_for_all_sessions(&self);

    async fn session_handle_waiting_for_load(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<SessionHandle>;

    fn code_nav_eligibility_for_request(
        &self,
        session_id: Option<&acp::SessionId>,
        cwd: &Path,
    ) -> Result<(), CodeNavEligibility>;

    fn start_codebase_index_for_code_nav(
        &self,
        session_id: Option<&acp::SessionId>,
        cwd: &Path,
    ) -> Option<(Arc<distill_codebase_graph::IndexManagerHandle>, bool)>;

    fn background_copy_context(&self) -> BackgroundCopyContext;

    fn session_registry_client(&self) -> Option<SessionRegistryClient>;

    fn resolve_workspace_ops(&self) -> Result<distill_workspace::WorkspaceOps, acp::Error>;

    async fn background_foreground_command(&self, session_id: &str, tool_call_id: &str) -> bool;
}
