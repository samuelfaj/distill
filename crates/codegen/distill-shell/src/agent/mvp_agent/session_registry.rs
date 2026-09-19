// Modified for Distill by Samuel Fajreldines, 2026.
//! Per-session resources and the registry that owns them.
//! Distinct from `agent::session_registry_client`, which talks to the remote registry.
use super::*;
use distill_tools::registry::types::FinalizedToolset;
/// The map stays private so every caller goes through a named operation.
#[derive(Clone, Default)]
pub(super) struct SessionRegistry {
    sessions: Rc<RefCell<HashMap<acp::SessionId, SessionResources>>>,
    pub(super) agent_directory: Rc<RefCell<super::agent_directory::AgentDirectory>>,
    next_install_id: Rc<std::cell::Cell<u64>>,
}
/// Whether an installed actor's root identity has reached disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IdentityStamp {
    Pending,
    Durable,
    Failed,
}
/// The actor an attach installed. The id is monotonic, so a stamp outcome lands on this incarnation
/// and never on a newer actor under the same session id; adopting attaches subscribe to `stamp`.
/// `bound_toolset` is the workspace binding the installer took for it, so a rollback releases exactly that.
pub(super) struct InstalledIncarnation {
    install_id: u64,
    stamp: tokio::sync::watch::Sender<IdentityStamp>,
    bound_toolset: Option<Arc<FinalizedToolset>>,
}
/// An install taken back or refused: the actor, its thread, and the workspace binding taken for it.
pub(super) struct WithdrawnInstall {
    pub(super) handle: SessionHandle,
    pub(super) thread: Option<SessionThread>,
    pub(super) bound_toolset: Option<Arc<FinalizedToolset>>,
}
impl WithdrawnInstall {
    /// A refused install was never bound: the installer binds only after the registry accepts.
    fn refused(handle: SessionHandle, thread: SessionThread) -> Box<Self> {
        Box::new(Self {
            handle,
            thread: Some(thread),
            bound_toolset: None,
        })
    }
}
/// What a stamp outcome did to the incarnation it was addressed to.
pub(super) enum StampResolution {
    Landed,
    Withdrawn(Box<WithdrawnInstall>),
    Superseded,
}
/// An install the registry refused, for the caller to shut down.
pub(super) type StaleInstall = Box<WithdrawnInstall>;
/// Turn activity of a resident actor.
/// Split from [`SessionLiveState`] so a reader cannot observe `Working` without a resident actor.
/// That combination was representable when liveness was a parallel field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Activity {
    Idle,
    Working,
}
/// Where a session is in its life. Each variant carries the evidence that makes it true, so a reader cannot observe a combination the type forbids.
/// A hosted actor is a handle on [`Self::Resident`] or mid-attach on [`Self::Attaching`].
/// [`Self::Attaching`] owns the waiter racing requests wait on and parks the previous presence in `displaced` so a failed attach can restore it. Terminal variants still carry an optional thread so `set_live` and `set_thread` cannot drop a thread the sweep still owns. `handle` is `Option` so `take_session` can drop residency without also rewriting live state (unload and close do that next).
pub(super) enum SessionPresence {
    /// Actor running and registered. The handle is the evidence of residency.
    Resident {
        handle: Option<SessionHandle>,
        thread: Option<SessionThread>,
        activity: Activity,
        root_identity: Option<super::agent_directory::PendingRootIdentity>,
        /// The hosted actor's outstanding stamp; an attach that settled ahead of it has no guard
        /// left, so the outcome lands on this record.
        incarnation: Option<InstalledIncarnation>,
    },
    /// A load or resume is building the actor.
    /// `waiter` wakes racing requests; `displaced` is what the attach replaced and what a failed attach restores.
    /// A handle, thread, or `settled_activity` may land here before the attach finishes; only [`SessionRegistry::settle_attach`] retires this variant.
    Attaching {
        waiter: tokio::sync::watch::Receiver<bool>,
        displaced: Option<Box<SessionPresence>>,
        handle: Option<SessionHandle>,
        thread: Option<SessionThread>,
        /// Activity a mid-attach `set_live` asked to apply once the attach settles.
        /// Independent liveness writes must not retire this variant.
        settled_activity: Option<Activity>,
        root_identity: Option<super::agent_directory::PendingRootIdentity>,
        /// The installed actor whose identity stamp is still outstanding.
        incarnation: Option<InstalledIncarnation>,
    },
    /// Client gone (`release` kept a flushing actor) with no live bit.
    Evicted {
        thread: Option<SessionThread>,
    },
    Closed {
        thread: Option<SessionThread>,
    },
    Dead {
        thread: Option<SessionThread>,
    },
    /// Idle-unloaded.
    /// Distinct from [`Self::Evicted`] because unload records `Dormant` while `release` records no live bit at all.
    Dormant {
        thread: Option<SessionThread>,
    },
}
impl SessionPresence {
    /// The roster and telemetry projection.
    /// `None` on [`Self::Evicted`]: `release` deliberately drops the live bit so a flushing thread is not reported as Dormant or Closed.
    pub(super) fn live_state(&self) -> Option<SessionLiveState> {
        match self {
            Self::Resident {
                activity: Activity::Working,
                ..
            } => Some(SessionLiveState::Working),
            Self::Resident {
                activity: Activity::Idle,
                ..
            } => Some(SessionLiveState::IdleResident),
            Self::Attaching { .. } => Some(SessionLiveState::Attaching),
            Self::Evicted { .. } => None,
            Self::Closed { .. } => Some(SessionLiveState::Completed),
            Self::Dead { .. } => Some(SessionLiveState::DeadFailed),
            Self::Dormant { .. } => Some(SessionLiveState::Dormant),
        }
    }
    fn from_live(
        state: SessionLiveState,
        thread: Option<SessionThread>,
        handle: Option<SessionHandle>,
        root_identity: Option<super::agent_directory::PendingRootIdentity>,
        incarnation: Option<InstalledIncarnation>,
    ) -> Self {
        match state {
            SessionLiveState::Working => Self::Resident {
                handle,
                thread,
                activity: Activity::Working,
                root_identity,
                incarnation,
            },
            SessionLiveState::IdleResident => Self::Resident {
                handle,
                thread,
                activity: Activity::Idle,
                root_identity,
                incarnation,
            },
            SessionLiveState::Attaching => {
                let (_tx, waiter) = tokio::sync::watch::channel(false);
                Self::Attaching {
                    waiter,
                    displaced: None,
                    handle,
                    thread,
                    settled_activity: None,
                    root_identity,
                    incarnation,
                }
            }
            SessionLiveState::Dormant => Self::Dormant { thread },
            SessionLiveState::Completed => Self::Closed { thread },
            SessionLiveState::DeadFailed => Self::Dead { thread },
        }
    }
    fn thread(&self) -> Option<&SessionThread> {
        if let Some(thread) = self.thread_slot().as_ref() {
            return Some(thread);
        }
        match self {
            Self::Attaching {
                displaced: Some(displaced),
                ..
            } => displaced.thread(),
            _ => None,
        }
    }
    fn has_thread(&self) -> bool {
        self.thread().is_some()
    }
    fn take_thread(&mut self) -> Option<SessionThread> {
        if let Some(thread) = self.thread_slot_mut().take() {
            return Some(thread);
        }
        match self {
            Self::Attaching {
                displaced: Some(displaced),
                ..
            } => displaced.take_thread(),
            _ => None,
        }
    }
    fn replace_thread(&mut self, thread: SessionThread) -> Option<SessionThread> {
        self.thread_slot_mut().replace(thread)
    }
    fn thread_slot(&self) -> &Option<SessionThread> {
        match self {
            Self::Resident { thread, .. }
            | Self::Attaching { thread, .. }
            | Self::Evicted { thread }
            | Self::Closed { thread }
            | Self::Dead { thread }
            | Self::Dormant { thread } => thread,
        }
    }
    fn thread_slot_mut(&mut self) -> &mut Option<SessionThread> {
        match self {
            Self::Resident { thread, .. }
            | Self::Attaching { thread, .. }
            | Self::Evicted { thread }
            | Self::Closed { thread }
            | Self::Dead { thread }
            | Self::Dormant { thread } => thread,
        }
    }
    fn hosted_handle(&self) -> Option<&SessionHandle> {
        match self {
            Self::Resident { handle, .. } | Self::Attaching { handle, .. } => handle.as_ref(),
            Self::Evicted { .. }
            | Self::Closed { .. }
            | Self::Dead { .. }
            | Self::Dormant { .. } => None,
        }
    }
    fn hosted_handle_mut(&mut self) -> Option<&mut SessionHandle> {
        match self {
            Self::Resident { handle, .. } | Self::Attaching { handle, .. } => handle.as_mut(),
            Self::Evicted { .. }
            | Self::Closed { .. }
            | Self::Dead { .. }
            | Self::Dormant { .. } => None,
        }
    }
    fn take_hosted_handle(&mut self) -> Option<SessionHandle> {
        match self {
            Self::Resident { handle, .. } | Self::Attaching { handle, .. } => handle.take(),
            Self::Evicted { .. }
            | Self::Closed { .. }
            | Self::Dead { .. }
            | Self::Dormant { .. } => None,
        }
    }
    fn root_identity(&self) -> Option<&super::agent_directory::PendingRootIdentity> {
        match self {
            Self::Resident { root_identity, .. } | Self::Attaching { root_identity, .. } => {
                root_identity.as_ref()
            }
            Self::Evicted { .. }
            | Self::Closed { .. }
            | Self::Dead { .. }
            | Self::Dormant { .. } => None,
        }
    }
    /// The stamp slot of the hosted actor, on either variant that hosts one.
    fn incarnation_slot_mut(&mut self) -> Option<&mut Option<InstalledIncarnation>> {
        match self {
            Self::Resident { incarnation, .. } | Self::Attaching { incarnation, .. } => {
                Some(incarnation)
            }
            Self::Evicted { .. }
            | Self::Closed { .. }
            | Self::Dead { .. }
            | Self::Dormant { .. } => None,
        }
    }
    /// The installed actor whose stamp is outstanding, only while its handle is still hosted here.
    fn installed(&self) -> Option<&InstalledIncarnation> {
        match self {
            Self::Resident {
                handle: Some(_),
                incarnation,
                ..
            }
            | Self::Attaching {
                handle: Some(_),
                incarnation,
                ..
            } => incarnation.as_ref(),
            Self::Resident { handle: None, .. }
            | Self::Attaching { handle: None, .. }
            | Self::Evicted { .. }
            | Self::Closed { .. }
            | Self::Dead { .. }
            | Self::Dormant { .. } => None,
        }
    }
    /// True when this presence holds nothing `drop_if_empty` should keep: an `Evicted` with no thread is today's `live = None, thread = None`.
    fn is_resource_empty(&self) -> bool {
        matches!(self, Self::Evicted { thread: None })
    }
}
/// The per-session state this registry owns: retained, resident resources, presence (thread and liveness), unavailable model, and bridge.
/// Load guards, rewind snapshots, local workspaces, and the handle map are owned elsewhere.
/// A new field belongs here only if `release` should drop it with the rest.
#[derive(Default)]
struct SessionResources {
    retained: Option<RetainedResources>,
    /// Cleared at idle-unload; survives a reload rebuild.
    resident: Option<ResidentResources>,
    presence: Option<SessionPresence>,
    /// Running actor threads that lost the presence's slot to another; the sweep drops each once it exits.
    retired_threads: Vec<SessionThread>,
    unavailable_model: Option<acp::ModelId>,
}
#[derive(Default)]
pub(super) struct SessionCounts {
    /// Tracked ids.
    /// The per-field counts below cannot see an entry that leaks with only a field they do not name.
    pub(super) entries: usize,
    pub(super) retained_resources: usize,
    pub(super) resident_resources: usize,
    pub(super) session_threads: usize,
    pub(super) session_live_state: usize,
    pub(super) model_unavailable_sessions: usize,
    pub(super) dispatch_locks: usize,
    pub(super) live_orphan_heal_locks: usize,
    pub(super) config_mutation_locks: usize,
    pub(super) session_turn_numbers: usize,
    pub(super) permission_event_receivers: usize,
    pub(super) session_index_claims: usize,
    pub(super) require_gateway_sessions: usize,
}
impl SessionRegistry {
    /// Releases everything a closing session leaves behind, in one drop.
    /// Running actor threads stay, the presence's and any retired: dropping a handle would detach its
    /// thread, and nothing would track the memory it holds. The sweep reclaims them later.
    pub(super) fn release(&self, id: &acp::SessionId) {
        let mut entries = self.sessions.borrow_mut();
        let Some(mut released) = entries.remove(id) else {
            return;
        };
        let running = released
            .presence
            .as_mut()
            .and_then(SessionPresence::take_thread)
            .filter(|t| !t.is_finished());
        let mut retired_threads = std::mem::take(&mut released.retired_threads);
        retired_threads.retain(|t| !t.is_finished());
        if running.is_some() || !retired_threads.is_empty() {
            entries.insert(
                id.clone(),
                SessionResources {
                    presence: Some(SessionPresence::Evicted { thread: running }),
                    retired_threads,
                    retained: None,
                    resident: None,
                    unavailable_model: None,
                },
            );
        }
        drop(entries);
        drop(released);
    }
    pub(super) fn set_thread(&self, id: &acp::SessionId, thread: SessionThread) {
        self.edit(id, |e| match &mut e.presence {
            Some(presence) => {
                let displaced = presence.replace_thread(thread);
                e.retire_running(id, displaced);
            }
            None => {
                e.presence = Some(SessionPresence::Evicted {
                    thread: Some(thread),
                });
            }
        });
    }
    /// Drops the tracked thread.
    /// Returns nothing on purpose: handing a `SessionThread` to a caller lets the last handle die in a local.
    /// That detaches the thread with no record left for the sweep.
    pub(super) fn clear_thread(&self, id: &acp::SessionId) {
        self.clear(id, |e| {
            if let Some(presence) = &mut e.presence {
                let _ = presence.take_thread();
                if presence.is_resource_empty() {
                    e.presence = None;
                }
            }
        });
    }
    /// `None` when no thread is tracked for the session.
    #[cfg(test)]
    pub(super) fn has_thread(&self, id: &acp::SessionId) -> bool {
        self.with(id, |e| {
            e.presence.as_ref().is_some_and(SessionPresence::has_thread)
        })
        .unwrap_or(false)
    }
    pub(super) fn thread_is_finished(&self, id: &acp::SessionId) -> Option<bool> {
        self.with(id, |e| {
            e.presence
                .as_ref()
                .and_then(SessionPresence::thread)
                .map(SessionThread::is_finished)
        })
        .flatten()
    }
    pub(super) fn finished_threads(&self) -> Vec<acp::SessionId> {
        self.sessions
            .borrow()
            .iter()
            .filter(|(_, e)| {
                e.presence
                    .as_ref()
                    .and_then(SessionPresence::thread)
                    .is_some_and(SessionThread::is_finished)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
    pub(super) fn clear_exited_thread(&self, id: &acp::SessionId) {
        self.clear(id, |e| {
            e.presence = None;
        });
    }
    /// Drop the retired threads that have exited; a running one stays tracked. An entry kept only
    /// for them goes with the last.
    pub(super) fn reap_retired_threads(&self) {
        let mut entries = self.sessions.borrow_mut();
        let mut emptied = Vec::new();
        for (id, entry) in entries.iter_mut() {
            let before = entry.retired_threads.len();
            entry.retired_threads.retain(|thread| !thread.is_finished());
            if entry.retired_threads.len() < before && entry.is_empty() {
                emptied.push(id.clone());
            }
        }
        for id in emptied {
            entries.remove(&id);
        }
    }
    pub(super) fn set_live(&self, id: &acp::SessionId, state: SessionLiveState) {
        if state == SessionLiveState::Attaching {
            tracing::warn!(session_id = %id.0, "ignoring set_live(Attaching) outside begin_attach");
            return;
        }
        self.edit(id, |e| {
            if let Some(SessionPresence::Attaching {
                settled_activity, ..
            }) = &mut e.presence
            {
                match state {
                    SessionLiveState::Working => {
                        *settled_activity = Some(Activity::Working);
                    }
                    SessionLiveState::IdleResident => {
                        *settled_activity = Some(Activity::Idle);
                    }
                    SessionLiveState::Attaching => {}
                    SessionLiveState::Dormant
                    | SessionLiveState::Completed
                    | SessionLiveState::DeadFailed => {
                        tracing::warn!(
                            session_id = %id.0,
                            ?state,
                            "set_live ignored a terminal write against an in-flight attach"
                        );
                    }
                }
                return;
            }
            let root_identity = e
                .presence
                .as_ref()
                .and_then(SessionPresence::root_identity)
                .cloned();
            let incarnation = e
                .presence
                .as_mut()
                .and_then(SessionPresence::incarnation_slot_mut)
                .and_then(Option::take);
            let thread = e.presence.as_mut().and_then(SessionPresence::take_thread);
            let handle = e
                .presence
                .as_mut()
                .and_then(SessionPresence::take_hosted_handle);
            e.presence = Some(SessionPresence::from_live(
                state,
                thread,
                handle,
                root_identity,
                incarnation,
            ));
        });
    }
    /// Hosted actor handle, if any.
    /// Mid-attach registration counts: the handle lands before the attach finishes, and callers already treat that as resident for lookup and sweep.
    pub(super) fn resident_handle(&self, id: &acp::SessionId) -> Option<SessionHandle> {
        self.with(id, |e| {
            e.presence
                .as_ref()
                .and_then(SessionPresence::hosted_handle)
                .cloned()
        })
        .flatten()
    }
    pub(super) fn is_resident(&self, id: &acp::SessionId) -> bool {
        self.with(id, |e| {
            e.presence
                .as_ref()
                .and_then(SessionPresence::hosted_handle)
                .is_some()
        })
        .unwrap_or(false)
    }
    pub(super) fn resident_count(&self) -> usize {
        self.sessions
            .borrow()
            .values()
            .filter(|e| {
                e.presence
                    .as_ref()
                    .and_then(SessionPresence::hosted_handle)
                    .is_some()
            })
            .count()
    }
    pub(super) fn resident_ids(&self) -> Vec<acp::SessionId> {
        self.sessions
            .borrow()
            .iter()
            .filter(|(_, e)| {
                e.presence
                    .as_ref()
                    .and_then(SessionPresence::hosted_handle)
                    .is_some()
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
    pub(super) fn for_each_resident(&self, mut f: impl FnMut(&acp::SessionId, &SessionHandle)) {
        for (id, entry) in self.sessions.borrow().iter() {
            if let Some(handle) = entry
                .presence
                .as_ref()
                .and_then(SessionPresence::hosted_handle)
            {
                f(id, handle);
            }
        }
    }
    pub(super) fn for_each_resident_mut(
        &self,
        mut f: impl FnMut(&acp::SessionId, &mut SessionHandle),
    ) {
        for (id, entry) in self.sessions.borrow_mut().iter_mut() {
            if let Some(handle) = entry
                .presence
                .as_mut()
                .and_then(SessionPresence::hosted_handle_mut)
            {
                f(id, handle);
            }
        }
    }
    pub(super) fn with_resident_mut<R>(
        &self,
        id: &acp::SessionId,
        f: impl FnOnce(&mut SessionHandle) -> R,
    ) -> Option<R> {
        let mut entries = self.sessions.borrow_mut();
        let handle = entries
            .get_mut(id)?
            .presence
            .as_mut()?
            .hosted_handle_mut()?;
        Some(f(handle))
    }
    /// Place a handle on the current presence without changing live kind.
    /// An entry with no presence becomes [`SessionPresence::Resident`] idle.
    /// Returns the displaced handle, if any, so the caller can reap it.
    pub(super) fn put_resident(
        &self,
        id: &acp::SessionId,
        handle: SessionHandle,
        root_identity: Option<super::agent_directory::PendingRootIdentity>,
    ) -> Option<SessionHandle> {
        self.put_resident_inner(id, handle, root_identity)
    }
    /// Returns the displaced handle and the install id the stamp outcome must be addressed to.
    pub(super) fn put_resident_for_attach(
        &self,
        id: &acp::SessionId,
        handle: SessionHandle,
        thread: SessionThread,
        root_identity: Option<super::agent_directory::PendingRootIdentity>,
        waiter: &tokio::sync::watch::Receiver<bool>,
    ) -> Result<(Option<SessionHandle>, u64), StaleInstall> {
        let mut entries = self.sessions.borrow_mut();
        let Some(entry) = entries.get_mut(id) else {
            return Err(WithdrawnInstall::refused(handle, thread));
        };
        let Some(SessionPresence::Attaching {
            waiter: current,
            handle: current_handle,
            thread: current_thread,
            root_identity: current_identity,
            incarnation,
            ..
        }) = &mut entry.presence
        else {
            return Err(WithdrawnInstall::refused(handle, thread));
        };
        if !current.same_channel(waiter) {
            return Err(WithdrawnInstall::refused(handle, thread));
        }
        if root_identity.is_some() {
            *current_identity = root_identity;
        }
        let install_id = self.next_install_id.get() + 1;
        self.next_install_id.set(install_id);
        *incarnation = Some(InstalledIncarnation {
            install_id,
            stamp: tokio::sync::watch::Sender::new(IdentityStamp::Pending),
            bound_toolset: None,
        });
        let displaced_handle = current_handle.replace(handle);
        let displaced_thread = current_thread.replace(thread);
        entry.retire_running(id, displaced_thread);
        drop(entries);
        self.agent_directory.borrow_mut().hide_attaching(id);
        Ok((displaced_handle, install_id))
    }
    /// Record the workspace binding the installer took for `install_id`, so withdrawing that install
    /// releases it, on whichever presence hosts the slot. A newer incarnation under the same id is never touched.
    pub(super) fn record_bound_toolset(
        &self,
        id: &acp::SessionId,
        install_id: u64,
        toolset: Arc<FinalizedToolset>,
    ) {
        let mut entries = self.sessions.borrow_mut();
        let installed = match entries
            .get_mut(id)
            .and_then(|entry| entry.presence.as_mut())
            .and_then(SessionPresence::incarnation_slot_mut)
        {
            Some(Some(installed)) if installed.install_id == install_id => installed,
            _ => {
                tracing::warn!(
                    session_id = %id.0,
                    install_id,
                    "bound toolset recorded for an install the registry no longer hosts"
                );
                return;
            }
        };
        installed.bound_toolset = Some(toolset);
    }
    /// Record the stamp outcome for one incarnation; a newer incarnation under the same id is never
    /// touched. Mid-attach the owning guard publishes or restores at settle; a record that already
    /// settled as `Resident` has no guard left, so it publishes or releases itself here.
    pub(super) fn resolve_identity_stamp(
        &self,
        id: &acp::SessionId,
        install_id: u64,
        outcome: IdentityStamp,
    ) -> StampResolution {
        let mut entries = self.sessions.borrow_mut();
        let Some(presence) = entries
            .get_mut(id)
            .and_then(|entry| entry.presence.as_mut())
        else {
            return StampResolution::Superseded;
        };
        let Some(installed) = presence
            .installed()
            .filter(|installed| installed.install_id == install_id)
        else {
            return StampResolution::Superseded;
        };
        installed.stamp.send_replace(outcome);
        match presence {
            SessionPresence::Attaching {
                handle,
                thread,
                root_identity,
                incarnation,
                displaced,
                ..
            } => {
                if outcome == IdentityStamp::Durable {
                    return StampResolution::Landed;
                }
                *root_identity = None;
                let bound_toolset = incarnation
                    .take()
                    .and_then(|installed| installed.bound_toolset);
                let Some(withdrawn) = handle.take() else {
                    return StampResolution::Superseded;
                };
                let mut thread = thread.take();
                if let Some(displaced) = displaced
                    && displaced
                        .hosted_handle()
                        .is_some_and(|h| h.cmd_tx.same_channel(&withdrawn.cmd_tx))
                {
                    let _ = displaced.take_hosted_handle();
                    if thread.is_none() {
                        thread = displaced.take_thread();
                    }
                }
                StampResolution::Withdrawn(Box::new(WithdrawnInstall {
                    handle: withdrawn,
                    thread,
                    bound_toolset,
                }))
            }
            SessionPresence::Resident {
                handle,
                thread,
                root_identity,
                incarnation,
                ..
            } => {
                let bound_toolset = incarnation
                    .take()
                    .and_then(|installed| installed.bound_toolset);
                if outcome == IdentityStamp::Durable {
                    let identity = root_identity.clone();
                    let is_resident = handle
                        .as_ref()
                        .is_some_and(|handle| !handle.cmd_tx.is_closed());
                    drop(entries);
                    self.agent_directory
                        .borrow_mut()
                        .settle_attach(id, identity, is_resident);
                    return StampResolution::Landed;
                }
                let Some(withdrawn) = handle.take() else {
                    return StampResolution::Superseded;
                };
                let thread = thread.take();
                *presence = SessionPresence::Evicted { thread: None };
                drop(entries);
                self.release_failed_load(id);
                StampResolution::Withdrawn(Box::new(WithdrawnInstall {
                    handle: withdrawn,
                    thread,
                    bound_toolset,
                }))
            }
            _ => StampResolution::Superseded,
        }
    }
    /// Drop what a load that produced nothing left behind: its directory row and the whole entry,
    /// retained state included (a lone `presence = None` would leave the turn counter behind).
    fn release_failed_load(&self, id: &acp::SessionId) {
        self.agent_directory
            .borrow_mut()
            .settle_attach(id, None, false);
        self.release(id);
    }
    /// The outstanding stamp of the installed actor an attach would adopt, with its install id.
    pub(super) fn pending_identity_stamp(
        &self,
        id: &acp::SessionId,
    ) -> Option<(u64, tokio::sync::watch::Receiver<IdentityStamp>)> {
        self.with(id, |entry| {
            let installed = entry.presence.as_ref()?.installed()?;
            Some((installed.install_id, installed.stamp.subscribe()))
        })
        .flatten()
    }
    #[cfg(test)]
    pub(super) fn identity_stamp_waiters(&self, id: &acp::SessionId) -> usize {
        self.with(id, |entry| {
            entry
                .presence
                .as_ref()
                .and_then(SessionPresence::installed)
                .map_or(0, |installed| installed.stamp.receiver_count())
        })
        .unwrap_or(0)
    }
    /// The identity to stamp for `install_id`, whether its attach is still in flight or a superseding
    /// guard already settled the record.
    pub(super) fn current_attach_identity(
        &self,
        id: &acp::SessionId,
        install_id: u64,
    ) -> Option<super::agent_directory::PendingRootIdentity> {
        self.with(id, |entry| {
            let presence = entry.presence.as_ref()?;
            presence
                .installed()
                .filter(|installed| installed.install_id == install_id)?;
            presence.root_identity().cloned()
        })
        .flatten()
    }
    fn put_resident_inner(
        &self,
        id: &acp::SessionId,
        handle: SessionHandle,
        root_identity: Option<super::agent_directory::PendingRootIdentity>,
    ) -> Option<SessionHandle> {
        let publish_identity = root_identity.clone();
        let displaced = self.edit(id, |e| {
            if let Some(slot) = e
                .presence
                .as_mut()
                .and_then(SessionPresence::incarnation_slot_mut)
            {
                *slot = None;
            }
            match &mut e.presence {
                Some(SessionPresence::Resident {
                    handle: existing,
                    root_identity: identity,
                    ..
                }) => {
                    *identity = root_identity;
                    existing.replace(handle)
                }
                Some(SessionPresence::Attaching {
                    handle: existing,
                    root_identity: identity,
                    ..
                }) => {
                    if root_identity.is_some() {
                        *identity = root_identity;
                    }
                    existing.replace(handle)
                }
                Some(presence) => {
                    let thread = presence.take_thread();
                    e.presence = Some(SessionPresence::Resident {
                        handle: Some(handle),
                        thread,
                        activity: Activity::Idle,
                        root_identity,
                        incarnation: None,
                    });
                    None
                }
                None => {
                    e.presence = Some(SessionPresence::Resident {
                        handle: Some(handle),
                        thread: None,
                        activity: Activity::Idle,
                        root_identity,
                        incarnation: None,
                    });
                    None
                }
            }
        });
        if self.is_attaching(id) {
            self.agent_directory.borrow_mut().hide_attaching(id);
            displaced
        } else {
            let is_resident = self
                .resident_handle(id)
                .is_some_and(|handle| !handle.cmd_tx.is_closed());
            self.agent_directory
                .borrow_mut()
                .settle_attach(id, publish_identity, is_resident);
            displaced
        }
    }
    /// Start an attach.
    /// Current presence becomes `displaced` so a failed attach can restore it.
    /// An attach over a missing entry creates one, which `settle_attach` removes.
    pub(super) fn begin_attach(
        &self,
        id: &acp::SessionId,
    ) -> (
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Receiver<bool>,
    ) {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let directory_identity =
            self.agent_directory
                .borrow_mut()
                .begin_attach(id)
                .map(|snapshot| {
                    super::agent_directory::PendingRootIdentity::from_snapshot(
                        snapshot,
                        crate::agent::roster::RosterOrigin::Local,
                    )
                });
        self.edit(id, |e| {
            let previous = e.presence.take();
            let (handle, thread, displaced, root_identity, incarnation) = match previous {
                Some(SessionPresence::Attaching {
                    handle,
                    thread,
                    displaced,
                    root_identity,
                    incarnation,
                    ..
                }) => (handle, thread, displaced, root_identity, incarnation),
                mut other => {
                    let handle = other
                        .as_ref()
                        .and_then(SessionPresence::hosted_handle)
                        .cloned();
                    let incarnation = other
                        .as_mut()
                        .and_then(SessionPresence::incarnation_slot_mut)
                        .and_then(Option::take);
                    let identity = match &other {
                        Some(SessionPresence::Resident { root_identity, .. }) => {
                            root_identity.clone()
                        }
                        _ => directory_identity.clone(),
                    };
                    (handle, None, other.map(Box::new), identity, incarnation)
                }
            };
            e.presence = Some(SessionPresence::Attaching {
                waiter: rx.clone(),
                displaced,
                handle,
                thread,
                settled_activity: None,
                root_identity,
                incarnation,
            });
        });
        (tx, rx)
    }
    /// Clone of the in-flight attach waiter, if any.
    pub(super) fn attach_waiter(
        &self,
        id: &acp::SessionId,
    ) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.with(id, |e| match &e.presence {
            Some(SessionPresence::Attaching { waiter, .. }) => Some(waiter.clone()),
            _ => None,
        })
        .flatten()
    }
    pub(super) fn is_attaching(&self, id: &acp::SessionId) -> bool {
        self.with(id, |e| {
            matches!(e.presence, Some(SessionPresence::Attaching { .. }))
        })
        .unwrap_or(false)
    }
    pub(super) fn attaching_count(&self) -> usize {
        self.sessions
            .borrow()
            .values()
            .filter(|e| matches!(e.presence, Some(SessionPresence::Attaching { .. })))
            .count()
    }
    /// Retire an attach: every guard drop lands here, success or failure.
    /// `waiter` identifies the owning guard; a superseded one is a no-op.
    /// Settles from the hosted handle when present, else restores `displaced`.
    pub(super) fn settle_attach(
        &self,
        id: &acp::SessionId,
        waiter: &tokio::sync::watch::Receiver<bool>,
    ) {
        let mut entries = self.sessions.borrow_mut();
        let Some(entry) = entries.get_mut(id) else {
            return;
        };
        let owns = match &entry.presence {
            Some(SessionPresence::Attaching {
                waiter: current, ..
            }) => current.same_channel(waiter),
            _ => false,
        };
        if !owns {
            return;
        }
        let Some(SessionPresence::Attaching {
            mut displaced,
            handle,
            thread,
            settled_activity,
            root_identity,
            incarnation,
            ..
        }) = entry.presence.take()
        else {
            return;
        };
        let running = handle.as_ref().map(|h| {
            h.current_prompt_id
                .lock()
                .map(|p| p.is_some())
                .unwrap_or(true)
        });
        let root_is_resident = handle
            .as_ref()
            .is_some_and(|handle| !handle.cmd_tx.is_closed());
        let incarnation =
            incarnation.filter(|installed| *installed.stamp.borrow() == IdentityStamp::Pending);
        let identity_is_durable = incarnation.is_none();
        entry.presence = if let Some(running) = running {
            let thread = match thread {
                Some(own) => {
                    entry.retire_running(id, displaced.as_mut().and_then(|d| d.take_thread()));
                    Some(own)
                }
                None => displaced.as_mut().and_then(|d| d.take_thread()),
            };
            let activity = match settled_activity {
                Some(activity) => activity,
                None if running => Activity::Working,
                None => Activity::Idle,
            };
            Some(SessionPresence::Resident {
                handle,
                thread,
                activity,
                root_identity: root_identity.clone(),
                incarnation,
            })
        } else if let Some(displaced) = displaced {
            let mut restored = match *displaced {
                SessionPresence::Attaching { thread, .. } => SessionPresence::Dormant { thread },
                SessionPresence::Resident { handle, thread, .. }
                    if handle.as_ref().is_none_or(|h| h.cmd_tx.is_closed()) =>
                {
                    SessionPresence::Dormant { thread }
                }
                other => other,
            };
            if let Some(own) = thread {
                if restored.thread_slot().is_none() {
                    let _ = restored.replace_thread(own);
                } else {
                    entry.retire_running(id, Some(own));
                }
            }
            Some(restored)
        } else {
            entry.presence = Some(SessionPresence::Evicted { thread });
            drop(entries);
            self.release_failed_load(id);
            return;
        };
        let settled_identity = root_identity.filter(|_| root_is_resident && identity_is_durable);
        drop(entries);
        self.agent_directory
            .borrow_mut()
            .settle_attach(id, settled_identity, root_is_resident);
        self.drop_if_empty(id);
    }
    /// Drop residency only.
    /// Live state and thread stay for the caller that owns the next transition (`set_live(Dormant)`, `release`, respawn).
    pub(super) fn take_resident(&self, id: &acp::SessionId) -> Option<SessionHandle> {
        let handle = self
            .sessions
            .borrow_mut()
            .get_mut(id)
            .and_then(|e| e.presence.as_mut())
            .and_then(|presence| {
                if let Some(slot) = presence.incarnation_slot_mut() {
                    *slot = None;
                }
                presence.take_hosted_handle()
            });
        self.drop_if_empty(id);
        handle
    }
    pub(super) fn live(&self, id: &acp::SessionId) -> Option<SessionLiveState> {
        self.with(id, |e| {
            e.presence.as_ref().and_then(SessionPresence::live_state)
        })
        .flatten()
    }
    pub(super) fn clear_resident(&self, id: &acp::SessionId) {
        self.clear(id, |e| e.resident = None);
    }
    pub(super) fn set_unavailable_model(&self, id: &acp::SessionId, model: acp::ModelId) {
        self.edit(id, |e| e.unavailable_model = Some(model));
    }
    pub(super) fn unavailable_model(&self, id: &acp::SessionId) -> Option<acp::ModelId> {
        self.with(id, |e| e.unavailable_model.clone()).flatten()
    }
    pub(super) fn take_unavailable_model(&self, id: &acp::SessionId) -> Option<acp::ModelId> {
        let model = self
            .sessions
            .borrow_mut()
            .get_mut(id)
            .and_then(|e| e.unavailable_model.take());
        self.drop_if_empty(id);
        model
    }
    pub(super) fn turn_number(&self, id: &acp::SessionId) -> Option<u64> {
        self.with(id, |e| e.retained.as_ref()?.turn_number)
            .flatten()
    }
    pub(super) fn set_turn_number(&self, id: &acp::SessionId, next: u64) {
        self.edit(id, |e| {
            e.retained.get_or_insert_default().turn_number = Some(next);
        });
    }
    pub(super) fn clear_turn_number(&self, id: &acp::SessionId) {
        self.clear(id, |e| {
            if let Some(retained) = &mut e.retained {
                retained.turn_number = None;
                if retained.is_empty() {
                    e.retained = None;
                }
            }
        });
    }
    pub(super) fn dispatch_lock(&self, id: &acp::SessionId) -> Rc<tokio::sync::Mutex<()>> {
        self.edit(id, |e| {
            e.retained
                .get_or_insert_default()
                .dispatch_lock
                .get_or_insert_with(Default::default)
                .clone()
        })
    }
    /// Per-parent heal mutex.
    /// Tray `list_running` and resume heal take this from the registry.
    /// The session actor holds the same `Arc` on `ToolContext` so overlapping ticks share one lock.
    pub(super) fn live_orphan_heal_lock(&self, id: &acp::SessionId) -> Arc<tokio::sync::Mutex<()>> {
        self.edit(id, |e| {
            e.retained
                .get_or_insert_default()
                .live_orphan_heal_lock
                .get_or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        })
    }
    /// Serializes a session's model and reasoning-effort changes so they compose.
    /// The handlers hold this for the whole change; see `handlers::model_switch`.
    pub(super) fn config_mutation_lock(&self, id: &acp::SessionId) -> Arc<tokio::sync::Mutex<()>> {
        self.edit(id, |e| {
            e.retained
                .get_or_insert_default()
                .config_mutation_lock
                .get_or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        })
    }
    pub(super) fn set_permission_receiver(
        &self,
        id: &acp::SessionId,
        rx: tokio::sync::mpsc::UnboundedReceiver<PermissionEvent>,
    ) {
        self.edit(id, |e| {
            e.retained.get_or_insert_default().permission_event_receiver = Some(rx);
        });
    }
    pub(super) fn drain_permission_events(&self, id: &acp::SessionId) -> Vec<PermissionEvent> {
        let mut events = Vec::new();
        let mut entries = self.sessions.borrow_mut();
        if let Some(rx) = entries
            .get_mut(id)
            .and_then(|e| e.retained.as_mut())
            .and_then(|r| r.permission_event_receiver.as_mut())
        {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        events
    }
    pub(super) fn set_codebase_index(
        &self,
        id: &acp::SessionId,
        index: std::sync::Arc<distill_codebase_graph::IndexManagerHandle>,
    ) {
        self.edit(id, |e| {
            e.resident.get_or_insert_default().codebase_index = Some(index);
        });
    }
    pub(super) fn mark_headless(&self, id: &acp::SessionId) {
        self.edit(id, |e| {
            e.resident.get_or_insert_default().is_headless = true;
        });
    }
    pub(super) fn is_headless(&self, id: &acp::SessionId) -> bool {
        self.with(id, |e| e.resident.as_ref().is_some_and(|r| r.is_headless))
            .unwrap_or(false)
    }
    /// Destructured so a new field has to be counted, or go unmeasured.
    pub(super) fn counts(&self) -> SessionCounts {
        let mut counts = SessionCounts::default();
        for entry in self.sessions.borrow().values() {
            counts.entries += 1;
            let SessionResources {
                retained,
                resident,
                presence,
                retired_threads,
                unavailable_model,
            } = entry;
            counts.retained_resources += usize::from(retained.is_some());
            counts.resident_resources += usize::from(resident.is_some());
            counts.session_threads +=
                usize::from(presence.as_ref().is_some_and(SessionPresence::has_thread))
                    + retired_threads.len();
            counts.session_live_state += usize::from(
                presence
                    .as_ref()
                    .and_then(SessionPresence::live_state)
                    .is_some(),
            );
            counts.model_unavailable_sessions += usize::from(unavailable_model.is_some());
            if let Some(retained) = retained {
                counts.dispatch_locks += usize::from(retained.dispatch_lock.is_some());
                counts.live_orphan_heal_locks +=
                    usize::from(retained.live_orphan_heal_lock.is_some());
                counts.config_mutation_locks +=
                    usize::from(retained.config_mutation_lock.is_some());
                counts.session_turn_numbers += usize::from(retained.turn_number.is_some());
                counts.permission_event_receivers +=
                    usize::from(retained.permission_event_receiver.is_some());
            }
            if let Some(resident) = resident {
                counts.session_index_claims += usize::from(resident.codebase_index.is_some());
                counts.require_gateway_sessions += usize::from(resident.require_gateway);
            }
        }
        counts
    }
    fn with<R>(&self, id: &acp::SessionId, f: impl FnOnce(&SessionResources) -> R) -> Option<R> {
        self.sessions.borrow().get(id).map(f)
    }
    fn edit<R>(&self, id: &acp::SessionId, f: impl FnOnce(&mut SessionResources) -> R) -> R {
        f(self.sessions.borrow_mut().entry(id.clone()).or_default())
    }
    fn clear(&self, id: &acp::SessionId, f: impl FnOnce(&mut SessionResources)) {
        {
            let mut entries = self.sessions.borrow_mut();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            f(entry);
        }
        self.drop_if_empty(id);
    }
    fn drop_if_empty(&self, id: &acp::SessionId) {
        let mut entries = self.sessions.borrow_mut();
        if entries.get(id).is_some_and(SessionResources::is_empty) {
            entries.remove(id);
        }
    }
}
impl SessionResources {
    /// The one rule for a thread displaced from a presence: a running one stays tracked for the
    /// sweep; a finished one has nothing left to track. Dropping a running `JoinHandle` detaches it.
    fn retire_running(&mut self, id: &acp::SessionId, thread: Option<SessionThread>) {
        if let Some(thread) = thread.filter(|thread| !thread.is_finished()) {
            tracing::debug!(session_id = %id.0, "session thread displaced while still running; retired for the sweep");
            self.retired_threads.push(thread);
        }
    }
    fn is_empty(&self) -> bool {
        let Self {
            retained,
            resident,
            presence,
            retired_threads,
            unavailable_model,
        } = self;
        let chat_vacant = true;
        let presence_vacant = match presence {
            None => true,
            Some(presence) => presence.is_resource_empty(),
        };
        retained.is_none()
            && resident.is_none()
            && presence_vacant
            && retired_threads.is_empty()
            && unavailable_model.is_none()
            && chat_vacant
    }
}
