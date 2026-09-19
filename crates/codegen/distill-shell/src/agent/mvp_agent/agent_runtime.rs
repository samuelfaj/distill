// Modified for Distill by Samuel Fajreldines, 2026.
//! `AgentRuntime` implementation for the run-loop agent.
//!
//! Each method delegates to `MvpAgent`'s inherent method of the same name;
//! inherent methods win name resolution, so these are delegations, not recursion.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol as acp;
use distill_acp_lib::AcpAgentGatewaySender as GatewaySender;
use distill_login::AuthManager;

use crate::agent::mvp_agent::MvpAgent;
use crate::agent::session_registry_client::SessionRegistryClient;
use crate::extensions::agent_runtime::AgentRuntime;
use crate::extensions::code_nav::CodeNavEligibility;
use crate::session::SessionHandle;
use crate::session::worktree::BackgroundCopyContext;
use crate::util::config::{RemoteSettings, WorktreeType};

#[async_trait::async_trait(?Send)]
impl AgentRuntime for MvpAgent {
    fn gateway(&self) -> &GatewaySender {
        &self.gateway
    }

    fn remote_settings(&self) -> Option<RemoteSettings> {
        self.cfg.borrow().remote_settings.clone()
    }

    fn auth_manager(&self) -> &Arc<AuthManager> {
        &self.auth_manager
    }

    fn worktree_type(&self) -> WorktreeType {
        self.worktree_type
    }

    fn restore_code(&self) -> bool {
        self.restore_code
    }

    fn get_session_cwd(&self, session_id: &acp::SessionId) -> Option<PathBuf> {
        self.get_session_cwd(session_id)
    }

    fn refresh_skill_baseline_for_all_sessions(&self) {
        self.refresh_skill_baseline_for_all_sessions();
    }

    async fn session_handle_waiting_for_load(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<SessionHandle> {
        self.session_handle_waiting_for_load(session_id).await
    }

    fn code_nav_eligibility_for_request(
        &self,
        session_id: Option<&acp::SessionId>,
        cwd: &Path,
    ) -> Result<(), CodeNavEligibility> {
        self.code_nav_eligibility_for_request(session_id, cwd)
    }

    fn start_codebase_index_for_code_nav(
        &self,
        session_id: Option<&acp::SessionId>,
        cwd: &Path,
    ) -> Option<(Arc<distill_codebase_graph::IndexManagerHandle>, bool)> {
        self.start_codebase_index_for_code_nav(session_id, cwd)
    }

    fn background_copy_context(&self) -> BackgroundCopyContext {
        self.background_copy_context()
    }

    fn session_registry_client(&self) -> Option<SessionRegistryClient> {
        self.session_registry_client()
    }

    fn resolve_workspace_ops(&self) -> Result<distill_workspace::WorkspaceOps, acp::Error> {
        self.resolve_workspace_ops()
    }

    async fn background_foreground_command(&self, session_id: &str, tool_call_id: &str) -> bool {
        self.background_foreground_command(session_id, tool_call_id)
            .await
    }
}
