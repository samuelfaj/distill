// Modified for Distill by Samuel Fajreldines, 2026.
//! Resident root identity indexed without retaining session handles.

use std::collections::HashMap;

use agent_client_protocol as acp;
use distill_tools::implementations::distill::task::root_control::AgentMessageGeneration;
use distill_message_delivery_core::{AgentId, AttemptId};

use crate::agent::roster::RosterOrigin;
use crate::session::persistence::SessionIdentity;

#[derive(Clone, Default)]
pub(super) struct AgentDirectory {
    by_agent: HashMap<AgentId, RootDirectoryEntry>,
    by_session: HashMap<acp::SessionId, AgentId>,
}

#[derive(Clone)]
pub(super) struct RootDirectoryEntry {
    agent_id: AgentId,
    session_id: acp::SessionId,
    attempt_id: AttemptId,
    generation: AgentMessageGeneration,
    state: RootDirectoryState,
    origin: RosterOrigin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RootDirectoryState {
    Attaching,
    Resident,
    Retiring,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RootDirectorySnapshot {
    pub(super) agent_id: AgentId,
    pub(super) session_id: acp::SessionId,
    pub(super) attempt_id: AttemptId,
    pub(super) generation: AgentMessageGeneration,
}

#[derive(Clone)]
pub(crate) struct PendingRootIdentity {
    pub(super) agent_id: AgentId,
    pub(super) attempt_id: AttemptId,
    pub(super) origin: RosterOrigin,
}

impl PendingRootIdentity {
    pub(super) fn parse(identity: SessionIdentity, origin: RosterOrigin) -> Option<Self> {
        Some(PendingRootIdentity {
            agent_id: AgentId::parse(&identity.agent_id)?,
            attempt_id: AttemptId::parse(&identity.attempt_id)?,
            origin,
        })
    }

    pub(super) fn from_snapshot(snapshot: RootDirectorySnapshot, origin: RosterOrigin) -> Self {
        PendingRootIdentity {
            agent_id: snapshot.agent_id,
            attempt_id: snapshot.attempt_id,
            origin,
        }
    }

    pub(super) fn to_session_identity(&self) -> SessionIdentity {
        SessionIdentity {
            agent_id: self.agent_id.to_string(),
            attempt_id: self.attempt_id.to_string(),
        }
    }
}

impl AgentDirectory {
    pub(super) fn register_root(
        &mut self,
        session_id: acp::SessionId,
        identity: PendingRootIdentity,
    ) -> RootDirectorySnapshot {
        if let Some(existing) = self.entry_mut(&session_id)
            && existing.agent_id == identity.agent_id
            && existing.attempt_id == identity.attempt_id
        {
            existing.state = RootDirectoryState::Resident;
            return existing.snapshot();
        }
        let generation = AgentMessageGeneration::mint(uuid::Uuid::now_v7().as_u128());
        self.remove_session(&session_id);
        if let Some(old) = self.by_agent.remove(&identity.agent_id) {
            self.by_session.remove(&old.session_id);
        }
        let entry = RootDirectoryEntry {
            agent_id: identity.agent_id.clone(),
            session_id: session_id.clone(),
            attempt_id: identity.attempt_id,
            generation,
            state: RootDirectoryState::Resident,
            origin: identity.origin,
        };
        let snapshot = entry.snapshot();
        self.by_session
            .insert(session_id, identity.agent_id.clone());
        self.by_agent.insert(identity.agent_id, entry);
        snapshot
    }

    pub(super) fn begin_attach(
        &mut self,
        session_id: &acp::SessionId,
    ) -> Option<RootDirectorySnapshot> {
        let entry = self.entry_mut(session_id)?;
        let snapshot = entry.snapshot();
        entry.state = RootDirectoryState::Attaching;
        Some(snapshot)
    }

    pub(super) fn hide_attaching(&mut self, session_id: &acp::SessionId) {
        if let Some(entry) = self.entry_mut(session_id) {
            entry.state = RootDirectoryState::Attaching;
        }
    }

    pub(super) fn settle_attach(
        &mut self,
        session_id: &acp::SessionId,
        identity: Option<PendingRootIdentity>,
        is_resident: bool,
    ) {
        match identity.filter(|_| is_resident) {
            Some(identity) => {
                self.register_root(session_id.clone(), identity);
            }
            None => self.remove_session(session_id),
        }
    }

    pub(super) fn retire_session(
        &mut self,
        session_id: &acp::SessionId,
    ) -> Option<RootDirectorySnapshot> {
        let entry = self.entry_mut(session_id)?;
        entry.state = RootDirectoryState::Retiring;
        Some(entry.snapshot())
    }

    pub(super) fn remove_retired_session(&mut self, session_id: &acp::SessionId) {
        if self
            .entry(session_id)
            .is_some_and(|e| e.state == RootDirectoryState::Retiring)
        {
            self.remove_session(session_id);
        }
    }

    #[cfg(test)]
    pub(super) fn root_for_agent(&self, agent_id: &AgentId) -> Option<RootDirectorySnapshot> {
        self.by_agent
            .get(agent_id)
            .and_then(RootDirectoryEntry::live)
    }

    #[cfg(test)]
    pub(super) fn root_for_session(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<RootDirectorySnapshot> {
        self.entry(session_id).and_then(RootDirectoryEntry::live)
    }

    #[cfg(test)]
    pub(super) fn snapshot_live(&self) -> Vec<RootDirectorySnapshot> {
        self.by_agent
            .values()
            .filter_map(RootDirectoryEntry::live)
            .collect()
    }

    fn entry(&self, id: &acp::SessionId) -> Option<&RootDirectoryEntry> {
        self.by_session
            .get(id)
            .and_then(|agent| self.by_agent.get(agent))
    }

    fn entry_mut(&mut self, id: &acp::SessionId) -> Option<&mut RootDirectoryEntry> {
        let agent = self.by_session.get(id)?.clone();
        self.by_agent.get_mut(&agent)
    }

    fn remove_session(&mut self, id: &acp::SessionId) {
        if let Some(agent) = self.by_session.remove(id) {
            self.by_agent.remove(&agent);
        }
    }
}

impl super::SessionRegistry {
    #[cfg(test)]
    pub(super) fn root_for_agent(&self, id: &AgentId) -> Option<RootDirectorySnapshot> {
        self.agent_directory.borrow().root_for_agent(id)
    }

    #[cfg(test)]
    pub(super) fn root_for_session(&self, id: &acp::SessionId) -> Option<RootDirectorySnapshot> {
        self.agent_directory.borrow().root_for_session(id)
    }

    #[cfg(test)]
    pub(super) fn snapshot_live_roots(&self) -> Vec<RootDirectorySnapshot> {
        self.agent_directory.borrow().snapshot_live()
    }

    #[cfg(test)]
    pub(super) fn register_root(
        &self,
        id: acp::SessionId,
        identity: PendingRootIdentity,
    ) -> RootDirectorySnapshot {
        self.agent_directory
            .borrow_mut()
            .register_root(id, identity)
    }

    pub(super) fn retire_root(&self, id: &acp::SessionId) -> Option<RootDirectorySnapshot> {
        self.agent_directory.borrow_mut().retire_session(id)
    }

    pub(super) fn remove_retired_root(&self, id: &acp::SessionId) {
        self.agent_directory.borrow_mut().remove_retired_session(id);
    }
}

impl super::MvpAgent {
    pub(super) fn retire_root_session(&self, session_id: &acp::SessionId) {
        drop(self.session_registry.retire_root(session_id));
    }
}

impl RootDirectoryEntry {
    fn snapshot(&self) -> RootDirectorySnapshot {
        RootDirectorySnapshot {
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            attempt_id: self.attempt_id.clone(),
            generation: self.generation,
        }
    }

    #[cfg_attr(not(test), expect(dead_code))]
    fn live(&self) -> Option<RootDirectorySnapshot> {
        (self.state == RootDirectoryState::Resident && self.origin == RosterOrigin::Local)
            .then(|| self.snapshot())
    }
}
