// Modified for Distill by Samuel Fajreldines, 2026.
//! Adapts live agents to the workspace membership controller.
use super::actions::Effect;
use super::agent::AgentId;
use super::agent_view::AgentView;
use super::app_view::AppView;
use super::workspace_layout::WorkspaceView;
use super::workspace_membership::{RemovalCause, RemovalRequestError, WorkspaceMembership};
use crate::views::dashboard::WorkspaceRowInputs;
use distill_dashboard_store::{
    MAX_CWD_BYTES, MAX_MODEL_BYTES, MAX_SUMMARY_BYTES, MAX_TITLE_BYTES, MemberKind, MemberMetadata,
    MemberOrigin, NewMember, SessionId,
};
use indexmap::IndexMap;
use std::collections::HashSet;
use std::time::UNIX_EPOCH;
pub(crate) fn request(app: &mut AppView) {
    if app.workspace_dashboard_enabled {
        app.workspace_membership.request_sync();
    }
}
pub(crate) fn activate(app: &mut AppView) {
    if app.workspace_dashboard_enabled {
        app.workspace_membership.activate();
    }
}
pub(crate) fn request_removal(
    app: &mut AppView,
    raw_session_id: &str,
    cause: RemovalCause,
) -> bool {
    if !app.workspace_dashboard_enabled {
        return true;
    }
    match app
        .workspace_membership
        .request_removal(raw_session_id, cause)
    {
        Ok(()) => true,
        Err(RemovalRequestError::ReadOnly) => {
            app.show_toast("Could not archive session: dashboard workspace is read-only");
            false
        }
        Err(RemovalRequestError::InvalidSessionId) => {
            tracing::warn!(
                session_id = raw_session_id,
                "invalid workspace archive session id"
            );
            app.show_toast("Could not archive session: invalid session id");
            false
        }
    }
}
pub(crate) fn permanent_delete_blocked(app: &AppView, raw_session_id: &str) -> bool {
    app.workspace_dashboard_enabled
        && app
            .workspace_membership
            .permanent_delete_blocked(raw_session_id)
}
pub(crate) fn allow_loaded_session(app: &mut AppView, raw_session_id: &str) {
    app.workspace_membership
        .on_explicit_session_load(raw_session_id);
}
pub(crate) fn refresh(app: &mut AppView) -> Vec<Effect> {
    app.workspace_membership.request_refresh().effects
}
pub(crate) fn live_session_ids(app: &AppView) -> HashSet<SessionId> {
    app.agents
        .values()
        .filter_map(|agent| agent.session.session_id.as_ref())
        .filter_map(|id| SessionId::new(id.0.to_string()).ok())
        .collect()
}
pub(crate) fn drain(app: &mut AppView) -> Vec<Effect> {
    if !app.workspace_dashboard_enabled {
        app.workspace_membership.disable();
        return Vec::new();
    }
    let candidates = if app.workspace_membership.wants_upsert_candidates() {
        let live_ids = live_session_ids(app);
        app.workspace_membership.retain_live_suppressions(&live_ids);
        let mut seen = HashSet::new();
        let home = app.home_session_agent;
        app.agents
            .iter()
            .filter(|(id, _)| home != Some(**id))
            .filter_map(|(_, agent)| agent_to_new_member(agent))
            .filter(|candidate| seen.insert(candidate.key.session_id.clone()))
            .collect()
    } else {
        Vec::new()
    };
    app.workspace_membership.next_effect(candidates).effects
}
/// Row inputs shared by the renderer and the dispatchers so they never see different rows.
#[derive(Default)]
pub(crate) struct WorkspaceRowSource {
    workspace: Option<WorkspaceView>,
    provisional: Vec<AgentId>,
}
impl WorkspaceRowSource {
    pub(crate) fn capture(
        agents: &IndexMap<AgentId, AgentView>,
        membership: &WorkspaceMembership,
        home: Option<AgentId>,
        workspace_dashboard_enabled: bool,
    ) -> Self {
        if !workspace_dashboard_enabled {
            return Self::default();
        }
        let workspace = membership.view();
        let provisional = provisional_agent_ids(agents, membership, home, workspace.as_ref());
        Self {
            workspace,
            provisional,
        }
    }
    pub(crate) fn inputs(&self) -> WorkspaceRowInputs<'_> {
        WorkspaceRowInputs {
            workspace: self.workspace.as_ref(),
            provisional: &self.provisional,
        }
    }
}
/// Live agents that render as provisional rows: the adoption rules minus the session id, which binds late, and not already covered by a committed member (that row renders through the member path under the same id).
/// A session with a removal in flight must not resurface, a bound id the store would reject can never persist, and an agent that lost its session to another agent (`clear_stale_session_id`) never rebinds, so none of those get a row.
fn provisional_agent_ids(
    agents: &IndexMap<AgentId, AgentView>,
    membership: &WorkspaceMembership,
    home: Option<AgentId>,
    workspace: Option<&WorkspaceView>,
) -> Vec<AgentId> {
    let is_member = |session_id: &SessionId| {
        workspace.is_some_and(|workspace| {
            workspace
                .members
                .iter()
                .any(|member| member.kind == MemberKind::Build && member.session_id == *session_id)
        })
    };
    agents
        .iter()
        .filter(|(id, agent)| home != Some(**id) && is_adoptable(agent))
        .filter(|(_, agent)| match agent.session.session_id.as_ref() {
            None => agent.session_binding_epoch == 0,
            Some(id) => SessionId::new(id.0.to_string())
                .is_ok_and(|id| !membership.is_session_hidden(&id) && !is_member(&id)),
        })
        .map(|(id, _)| *id)
        .collect()
}
/// Whether `agent` can ever become a workspace member; the session id is checked separately because it binds late.
fn is_adoptable(agent: &AgentView) -> bool {
    let cwd = agent.session.cwd.to_string_lossy();
    if agent.conversation_entry || !agent.session.cwd.is_absolute() || cwd.len() > MAX_CWD_BYTES {
        return false;
    }
    true
}
fn agent_to_new_member(agent: &AgentView) -> Option<NewMember> {
    if !is_adoptable(agent) {
        return None;
    }
    if agent.session.created_via_new && crate::views::dashboard::row::is_empty_idle_top_level(agent)
    {
        return None;
    }
    let session_id = SessionId::new(agent.session.session_id.as_ref()?.0.to_string()).ok()?;
    Some(NewMember {
        key: distill_dashboard_store::MemberKey {
            session_id,
            kind: MemberKind::Build,
        },
        origin: MemberOrigin::Local,
        metadata: agent_metadata(agent),
    })
}
fn agent_metadata(agent: &AgentView) -> MemberMetadata {
    let state = crate::views::dashboard::classify_top_level(agent);
    let last_change = crate::views::dashboard::row::top_level_last_change_at(agent, state);
    let last_change_unix_ms = last_change
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_mul(1_000)
        .try_into()
        .unwrap_or(i64::MAX);
    MemberMetadata {
        cwd: Some(agent.session.cwd.display().to_string()),
        title: truncate(
            crate::views::session_title::rename_source_title(agent),
            MAX_TITLE_BYTES,
        ),
        model: truncate(
            agent
                .session
                .models
                .current_model_id_str()
                .map(str::to_owned),
            MAX_MODEL_BYTES,
        ),
        last_turn_summary: truncate(agent.last_turn_summary.clone(), MAX_SUMMARY_BYTES),
        is_worktree: agent.is_worktree || agent.session.is_worktree,
        last_change_unix_ms,
    }
}
fn truncate(value: Option<String>, max_bytes: usize) -> Option<String> {
    value.map(|mut value| {
        if value.len() > max_bytes {
            let mut end = max_bytes;
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            value.truncate(end);
        }
        value
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol as acp;
    fn eligible_agent() -> AgentView {
        let mut agent = crate::app::agent_view::test_fixtures::make_agent();
        agent.session.session_id = Some(acp::SessionId::new("saved"));
        agent.session.cwd = "/tmp/workspace-sync".into();
        agent.display_name = Some("Saved session".into());
        agent
    }
    #[test]
    fn maps_bound_local_build_agent_metadata() {
        let mut agent = eligible_agent();
        agent.display_name = Some("Saved title".into());
        agent.last_turn_summary = Some("Summary".into());
        agent.session.is_worktree = true;
        let member = agent_to_new_member(&agent).expect("eligible build agent");
        assert_eq!(member.key.session_id.as_ref(), "saved");
        assert!(matches!(member.key.kind, MemberKind::Build));
        assert!(matches!(member.origin, MemberOrigin::Local));
        assert_eq!(member.metadata.cwd.as_deref(), Some("/tmp/workspace-sync"));
        assert_eq!(member.metadata.title.as_deref(), Some("Saved title"));
        assert_eq!(
            member.metadata.last_turn_summary.as_deref(),
            Some("Summary")
        );
        assert!(member.metadata.is_worktree);
    }
    #[test]
    fn skips_unbound_conversation_and_relative_cwd_agents() {
        let mut agent = crate::app::agent_view::test_fixtures::make_agent();
        agent.session.cwd = "/tmp/workspace-sync".into();
        assert!(agent_to_new_member(&agent).is_none());
        agent.session.session_id = Some(acp::SessionId::new("saved"));
        agent.conversation_entry = true;
        assert!(agent_to_new_member(&agent).is_none());
        agent.conversation_entry = false;
        agent.session.cwd = "relative".into();
        assert!(agent_to_new_member(&agent).is_none());
    }
    #[test]
    fn skips_bound_empty_idle_startup_agent() {
        let mut agent = eligible_agent();
        agent.display_name = None;
        agent.session.created_via_new = true;
        assert!(agent_to_new_member(&agent).is_none());
    }
    #[test]
    fn provisional_ids_cover_unbound_dispatch_but_not_hidden_or_ineligible_agents() {
        let mut agents = IndexMap::new();
        let mut dispatched = crate::app::agent_view::test_fixtures::make_agent();
        dispatched.session.cwd = "/tmp/workspace-sync".into();
        dispatched.session.enqueue_prompt("fix the bug".into());
        assert!(dispatched.session.session_id.is_none());
        agents.insert(AgentId(1), dispatched);
        let mut conversation = eligible_agent();
        conversation.conversation_entry = true;
        agents.insert(AgentId(2), conversation);
        let mut archived = eligible_agent();
        archived.session.session_id = Some(acp::SessionId::new("archived"));
        agents.insert(AgentId(3), archived);
        agents.insert(AgentId(4), eligible_agent());
        agents.insert(
            AgentId(5),
            crate::app::agent_view::test_fixtures::make_agent(),
        );
        let mut unstorable = eligible_agent();
        unstorable.session.session_id = Some(acp::SessionId::new("has/separator"));
        agents.insert(AgentId(6), unstorable);
        let mut stale = eligible_agent();
        stale.unbind_session_id();
        assert!(stale.session.session_id.is_none());
        agents.insert(AgentId(7), stale);
        let mut committed = eligible_agent();
        committed.session.session_id = Some(acp::SessionId::new("committed"));
        agents.insert(AgentId(8), committed);
        let mut membership = WorkspaceMembership::default();
        membership.suppress_for_test(SessionId::new("archived").unwrap());
        membership.set_snapshot_for_test(crate::app::workspace_test_fixtures::snapshot(vec![
            crate::app::workspace_test_fixtures::member("committed", "Committed"),
        ]));
        assert_eq!(
            provisional_agent_ids(
                &agents,
                &membership,
                Some(AgentId(5)),
                membership.view().as_ref()
            ),
            vec![AgentId(1), AgentId(4)],
            "the committed agent renders through its member, not as a second row"
        );
        assert!(
            WorkspaceRowSource::capture(
                &agents,
                &membership,
                Some(AgentId(5)),
                /* workspace_dashboard_enabled */ false
            )
            .inputs()
            .provisional
            .is_empty(),
            "v1 must not pay for the provisional walk"
        );
    }
    #[test]
    fn metadata_is_canonicalized_to_store_limits() {
        let mut agent = eligible_agent();
        agent.display_name = Some("é".repeat(MAX_TITLE_BYTES));
        agent.last_turn_summary = Some("s".repeat(MAX_SUMMARY_BYTES + 10));
        let member = agent_to_new_member(&agent).unwrap();
        assert!(member.metadata.title.unwrap().len() <= MAX_TITLE_BYTES);
        assert_eq!(
            member.metadata.last_turn_summary.unwrap().len(),
            MAX_SUMMARY_BYTES
        );
    }
}
