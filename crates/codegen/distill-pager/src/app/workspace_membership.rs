// Modified for Distill by Samuel Fajreldines, 2026.
//! Owns workspace persistence and mutation policy without depending on `AppView`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use distill_dashboard_store::{
    LayoutApplyOutcome, LayoutPatch, MemberKey, MemberKind, MemberMetadata, NewMember, SchemaState,
    SessionId, WORKSPACE_CAPACITY, WorkspaceSnapshot, WorkspaceStore,
};

use super::actions::{
    Effect, WorkspaceMutation, WorkspaceMutationFailure, WorkspaceWriteCompletion,
};
use super::workspace_layout::{
    LayoutCompletion, LayoutState, WorkspaceGrouping, WorkspaceView, member_key,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StoreAccess {
    ReadWrite,
    ReadOnly,
}

#[derive(Debug)]
enum WorkspaceIo {
    Unopened,
    Opening {
        db_path: PathBuf,
        attempt: OpenAttempt,
        access: Option<StoreAccess>,
    },
    Ready {
        store: WorkspaceStore,
        access: StoreAccess,
    },
    InFlight {
        db_path: PathBuf,
        access: StoreAccess,
    },
    AwaitingRetry {
        db_path: PathBuf,
        access: Option<StoreAccess>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAttempt {
    Initial,
    Retried,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemovalCause {
    Archive,
    HistoryDeletedWithRetainedView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryBudget {
    Fresh,
    Retried,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RemovalState {
    Pending {
        completion: RemovalCompletion,
        retry: RetryBudget,
    },
    SuppressedUntilExplicitLoad {
        cause: SuppressionCause,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuppressionCause {
    HistoryDeleted,
    SnapshotRemoval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemovalCompletion {
    Forget,
    SuppressUntilExplicitLoad,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UpsertAttempt {
    Retried(MemberMetadata),
    Failed(MemberMetadata),
}

enum MembershipWrite {
    Upsert(Vec<NewMember>),
    Remove(Vec<MemberKey>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemovalRequestError {
    ReadOnly,
    InvalidSessionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LayoutRequestError {
    ReadOnly,
    MemberNotFound,
}

#[derive(Debug)]
pub(crate) enum WorkspaceNotice {
    ArchiveRejectedReadOnly,
    LoadFailed {
        error: String,
    },
    Refreshing {
        error: String,
    },
    SyncFailed {
        count: usize,
        session_id: String,
        error: String,
    },
    ArchiveFailed {
        count: usize,
        session_id: String,
        error: String,
    },
    ReadOnly,
    WriterFailed {
        error: String,
    },
    LayoutFailed {
        error: String,
    },
    RefreshFailed {
        error: String,
    },
}

#[derive(Debug, Default)]
pub(crate) struct WorkspaceTransition {
    pub effects: Vec<Effect>,
    pub notices: Vec<WorkspaceNotice>,
}

#[derive(Debug)]
pub(crate) struct WorkspaceMembership {
    io: WorkspaceIo,
    committed_snapshot: Option<WorkspaceSnapshot>,
    layout: LayoutState,
    dirty: bool,
    /// A live-agent candidate scan still owed after higher-priority writes.
    sync_pending: bool,
    removals: HashMap<SessionId, RemovalState>,
    upserts: HashMap<SessionId, UpsertAttempt>,
    explicit_adoptions: HashSet<SessionId>,
}

impl Default for WorkspaceMembership {
    fn default() -> Self {
        Self {
            io: WorkspaceIo::Unopened,
            committed_snapshot: None,
            layout: LayoutState::default(),
            dirty: false,
            sync_pending: false,
            removals: HashMap::new(),
            upserts: HashMap::new(),
            explicit_adoptions: HashSet::new(),
        }
    }
}

impl WorkspaceMembership {
    pub fn snapshot(&self) -> Option<&WorkspaceSnapshot> {
        self.committed_snapshot.as_ref()
    }

    pub fn view(&self) -> Option<WorkspaceView> {
        let snapshot = self.committed_snapshot.as_ref()?;
        Some(self.layout.view(snapshot, &self.pending_removal_key_set()))
    }

    pub fn writes_disabled(&self) -> bool {
        matches!(
            self.io,
            WorkspaceIo::Ready {
                access: StoreAccess::ReadOnly,
                ..
            } | WorkspaceIo::InFlight {
                access: StoreAccess::ReadOnly,
                ..
            } | WorkspaceIo::Opening {
                access: Some(StoreAccess::ReadOnly),
                ..
            } | WorkspaceIo::AwaitingRetry {
                access: Some(StoreAccess::ReadOnly),
                ..
            }
        )
    }

    pub fn activate(&mut self) {
        self.dirty = true;
        self.sync_pending = true;
    }

    pub fn request_sync(&mut self) {
        if !matches!(
            self.io,
            WorkspaceIo::Unopened | WorkspaceIo::AwaitingRetry { .. }
        ) {
            self.dirty = true;
            self.sync_pending = true;
        }
    }

    pub fn request_pin(&mut self, key: MemberKey, pinned: bool) -> Result<(), LayoutRequestError> {
        self.validate_layout_keys(std::slice::from_ref(&key))?;
        self.layout.set_pinned(key, pinned);
        self.dirty = true;
        Ok(())
    }

    pub fn request_manual_order(&mut self, keys: Vec<MemberKey>) -> Result<(), LayoutRequestError> {
        self.validate_layout_keys(&keys)?;
        self.layout.set_manual_order(keys);
        self.dirty = true;
        Ok(())
    }

    pub fn request_grouping(
        &mut self,
        grouping: WorkspaceGrouping,
    ) -> Result<(), LayoutRequestError> {
        self.validate_layout_keys(&[])?;
        self.layout.set_grouping(grouping);
        self.dirty = true;
        Ok(())
    }

    pub fn effective_pinned(&self, key: &MemberKey) -> Option<bool> {
        self.view()?
            .members
            .iter()
            .find(|member| member_key(member) == *key)
            .map(|member| member.pin_rank.is_some())
    }

    pub fn effective_manual_order(&self) -> Vec<MemberKey> {
        self.view()
            .map(|workspace| workspace.manual_order())
            .unwrap_or_default()
    }

    pub fn effective_grouping(&self) -> Option<WorkspaceGrouping> {
        self.view().map(|workspace| workspace.grouping)
    }

    pub fn disable(&mut self) {
        self.dirty = false;
        self.sync_pending = false;
        self.layout.clear();
        self.removals
            .retain(|_, state| matches!(state, RemovalState::SuppressedUntilExplicitLoad { .. }));
    }

    pub fn request_removal(
        &mut self,
        raw_session_id: &str,
        cause: RemovalCause,
    ) -> Result<(), RemovalRequestError> {
        if self.writes_disabled() {
            return Err(RemovalRequestError::ReadOnly);
        }
        let session_id = SessionId::new(raw_session_id.to_owned())
            .map_err(|_| RemovalRequestError::InvalidSessionId)?;
        let completion = match cause {
            RemovalCause::Archive => RemovalCompletion::Forget,
            RemovalCause::HistoryDeletedWithRetainedView => {
                RemovalCompletion::SuppressUntilExplicitLoad
            }
        };
        self.removals.insert(
            session_id,
            RemovalState::Pending {
                completion,
                retry: RetryBudget::Fresh,
            },
        );
        self.dirty = true;
        Ok(())
    }

    /// A session with an in-flight or suppressed removal must not resurface as a live row.
    pub fn is_session_hidden(&self, session_id: &SessionId) -> bool {
        self.removals.contains_key(session_id)
    }

    pub fn permanent_delete_blocked(&self, raw_session_id: &str) -> bool {
        if !self.writes_disabled() {
            return false;
        }
        let Ok(session_id) = SessionId::new(raw_session_id.to_owned()) else {
            return false;
        };
        self.committed_snapshot.as_ref().is_some_and(|snapshot| {
            snapshot.members.iter().any(|member| {
                member.session_id == session_id && matches!(member.kind, MemberKind::Build)
            })
        })
    }

    pub fn on_explicit_session_load(&mut self, raw_session_id: &str) {
        let Ok(session_id) = SessionId::new(raw_session_id.to_owned()) else {
            return;
        };
        if matches!(self.io, WorkspaceIo::Unopened) && !self.removals.contains_key(&session_id) {
            return;
        }
        self.explicit_adoptions.insert(session_id.clone());
        self.dirty = true;
        self.sync_pending = true;
        match self.removals.get_mut(&session_id) {
            Some(RemovalState::Pending { completion, .. }) => {
                *completion = RemovalCompletion::Forget;
            }
            Some(RemovalState::SuppressedUntilExplicitLoad { .. }) => {
                self.removals.remove(&session_id);
            }
            None => {}
        }
    }

    pub fn retain_live_suppressions(&mut self, live_ids: &HashSet<SessionId>) {
        self.removals.retain(|session_id, state| {
            !matches!(state, RemovalState::SuppressedUntilExplicitLoad { .. })
                || live_ids.contains(session_id)
        });
        self.explicit_adoptions
            .retain(|session_id| live_ids.contains(session_id));
    }

    pub fn wants_upsert_candidates(&self) -> bool {
        self.sync_pending
            && matches!(
                self.io,
                WorkspaceIo::Ready {
                    access: StoreAccess::ReadWrite,
                    ..
                }
            )
    }

    pub fn next_effect(&mut self, candidates: Vec<NewMember>) -> WorkspaceTransition {
        if !self.dirty {
            return WorkspaceTransition::default();
        }

        match &self.io {
            WorkspaceIo::Opening { .. } | WorkspaceIo::InFlight { .. } => {
                return WorkspaceTransition::default();
            }
            WorkspaceIo::Ready {
                access: StoreAccess::ReadOnly,
                ..
            } => {
                self.dirty = false;
                self.sync_pending = false;
                return WorkspaceTransition::default();
            }
            WorkspaceIo::Unopened | WorkspaceIo::AwaitingRetry { .. } => {
                let (db_path, access) = match &self.io {
                    WorkspaceIo::AwaitingRetry {
                        db_path, access, ..
                    } => (db_path.clone(), *access),
                    _ => (
                        distill_dashboard_store::default_db_path(&distill_config::distill_home()),
                        None,
                    ),
                };
                self.io = WorkspaceIo::Opening {
                    db_path: db_path.clone(),
                    attempt: OpenAttempt::Initial,
                    access,
                };
                return WorkspaceTransition {
                    effects: vec![Effect::LoadWorkspaceSnapshot { db_path }],
                    notices: Vec::new(),
                };
            }
            WorkspaceIo::Ready { .. } => {}
        }

        if let Some(snapshot) = self.committed_snapshot.as_ref() {
            self.layout.reconcile_committed(snapshot);
        }
        let pending_removal_keys = self.pending_removal_keys();
        let mutation = if !pending_removal_keys.is_empty() {
            WorkspaceMutation::Remove(pending_removal_keys)
        } else if let Some(snapshot) = self.committed_snapshot.as_ref() {
            let patch = self.layout.plan(snapshot, &self.pending_removal_key_set());
            if patch.is_empty() {
                let members = self.plan_upserts(candidates);
                if members.is_empty() {
                    self.sync_pending = false;
                    self.dirty = false;
                    return WorkspaceTransition::default();
                }
                WorkspaceMutation::Upsert(members)
            } else {
                WorkspaceMutation::Layout(patch)
            }
        } else {
            let members = self.plan_upserts(candidates);
            if members.is_empty() {
                self.sync_pending = false;
                self.dirty = false;
                return WorkspaceTransition::default();
            }
            WorkspaceMutation::Upsert(members)
        };
        let io = std::mem::replace(&mut self.io, WorkspaceIo::Unopened);
        let WorkspaceIo::Ready {
            store,
            access: StoreAccess::ReadWrite,
        } = io
        else {
            self.io = io;
            return WorkspaceTransition::default();
        };
        let db_path = store.path().to_path_buf();
        self.io = WorkspaceIo::InFlight {
            db_path,
            access: StoreAccess::ReadWrite,
        };
        if matches!(&mutation, WorkspaceMutation::Upsert(_)) {
            self.sync_pending = false;
        }
        self.dirty = self.sync_pending;
        WorkspaceTransition {
            effects: vec![Effect::WriteWorkspace { store, mutation }],
            notices: Vec::new(),
        }
    }

    pub fn on_store_opened(
        &mut self,
        store: WorkspaceStore,
        snapshot: WorkspaceSnapshot,
        live_ids: &HashSet<SessionId>,
    ) -> WorkspaceTransition {
        let access = access_for(&store);
        let rejected_archive = self.publish_snapshot(access, snapshot, live_ids);
        self.io = WorkspaceIo::Ready { store, access };
        self.upserts.clear();
        WorkspaceTransition {
            effects: Vec::new(),
            notices: rejected_archive
                .then_some(WorkspaceNotice::ArchiveRejectedReadOnly)
                .into_iter()
                .collect(),
        }
    }

    pub fn on_store_open_failed(&mut self, error: String, retryable: bool) -> WorkspaceTransition {
        let (db_path, attempt, access) = match &self.io {
            WorkspaceIo::Opening {
                db_path,
                attempt,
                access,
            } => (db_path.clone(), *attempt, *access),
            WorkspaceIo::InFlight {
                db_path, access, ..
            } => (db_path.clone(), OpenAttempt::Retried, Some(*access)),
            WorkspaceIo::AwaitingRetry {
                db_path, access, ..
            } => (db_path.clone(), OpenAttempt::Retried, *access),
            WorkspaceIo::Ready { store, access } => (
                store.path().to_path_buf(),
                OpenAttempt::Retried,
                Some(*access),
            ),
            WorkspaceIo::Unopened => (
                distill_dashboard_store::default_db_path(&distill_config::distill_home()),
                OpenAttempt::Retried,
                None,
            ),
        };
        let retry =
            retryable && matches!(attempt, OpenAttempt::Initial) && !self.has_pending_removals();
        let effects = if retry {
            self.io = WorkspaceIo::Opening {
                db_path: db_path.clone(),
                attempt: OpenAttempt::Retried,
                access,
            };
            vec![Effect::LoadWorkspaceSnapshot { db_path }]
        } else {
            self.io = WorkspaceIo::AwaitingRetry { db_path, access };
            self.dirty = false;
            Vec::new()
        };
        WorkspaceTransition {
            effects,
            notices: if retry {
                Vec::new()
            } else {
                vec![WorkspaceNotice::LoadFailed { error }]
            },
        }
    }

    pub fn on_write_completed(
        &mut self,
        store: WorkspaceStore,
        completion: WorkspaceWriteCompletion,
        live_ids: &HashSet<SessionId>,
    ) -> WorkspaceTransition {
        let (snapshot, failures, write) = match completion {
            WorkspaceWriteCompletion::Layout { patch, outcome } => {
                return self.complete_layout(store, patch, outcome, live_ids);
            }
            WorkspaceWriteCompletion::Upsert {
                members,
                snapshot,
                failures,
            } => (snapshot, failures, MembershipWrite::Upsert(members)),
            WorkspaceWriteCompletion::Remove {
                keys,
                snapshot,
                failures,
            } => (snapshot, failures, MembershipWrite::Remove(keys)),
        };
        let db_path = store.path().to_path_buf();
        let access = access_for(&store);
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.io = WorkspaceIo::Opening {
                    db_path: db_path.clone(),
                    attempt: OpenAttempt::Initial,
                    access: Some(access),
                };
                self.dirty = true;
                self.sync_pending = true;
                return WorkspaceTransition {
                    effects: vec![Effect::LoadWorkspaceSnapshot { db_path }],
                    notices: vec![WorkspaceNotice::Refreshing { error }],
                };
            }
        };
        let mut transition = match write {
            MembershipWrite::Upsert(members) => self.complete_upserts(members, &failures),
            MembershipWrite::Remove(keys) => self.complete_removals(keys, &failures),
        };
        let rejected_archive = self.publish_snapshot(access, snapshot, live_ids);
        if rejected_archive {
            transition
                .notices
                .push(WorkspaceNotice::ArchiveRejectedReadOnly);
        }
        if access == StoreAccess::ReadOnly {
            transition.notices.push(WorkspaceNotice::ReadOnly);
        }
        self.io = WorkspaceIo::Ready { store, access };
        transition
    }

    fn complete_layout(
        &mut self,
        store: WorkspaceStore,
        patch: LayoutPatch,
        outcome: LayoutApplyOutcome,
        live_ids: &HashSet<SessionId>,
    ) -> WorkspaceTransition {
        let completion = self.layout.complete(&patch, outcome);
        let (access, notices) = match completion {
            LayoutCompletion::Continue {
                snapshot,
                retry,
                failure,
            } => {
                let access = access_for(&store);
                if let Some(snapshot) = snapshot {
                    self.publish_snapshot(access, snapshot, live_ids);
                }
                self.dirty |= retry;
                (
                    access,
                    failure
                        .map(|error| WorkspaceNotice::LayoutFailed { error })
                        .into_iter()
                        .collect(),
                )
            }
            LayoutCompletion::DisableWrites { snapshot } => {
                if let Some(snapshot) = snapshot {
                    self.publish_snapshot(StoreAccess::ReadOnly, snapshot, live_ids);
                }
                self.removals.retain(|_, state| {
                    matches!(state, RemovalState::SuppressedUntilExplicitLoad { .. })
                });
                self.upserts.clear();
                self.dirty = false;
                self.sync_pending = false;
                (StoreAccess::ReadOnly, vec![WorkspaceNotice::ReadOnly])
            }
        };
        self.io = WorkspaceIo::Ready { store, access };
        WorkspaceTransition {
            effects: Vec::new(),
            notices,
        }
    }

    pub fn request_refresh(&mut self) -> WorkspaceTransition {
        if self.dirty || self.sync_pending || self.has_pending_removals() || !self.layout.is_empty()
        {
            return WorkspaceTransition::default();
        }
        let Some(known_data_version) = self
            .committed_snapshot
            .as_ref()
            .map(|snapshot| snapshot.data_version)
        else {
            return WorkspaceTransition::default();
        };
        let io = std::mem::replace(&mut self.io, WorkspaceIo::Unopened);
        let WorkspaceIo::Ready { store, access } = io else {
            self.io = io;
            return WorkspaceTransition::default();
        };
        let db_path = store.path().to_path_buf();
        self.io = WorkspaceIo::InFlight { db_path, access };
        WorkspaceTransition {
            effects: vec![Effect::RefreshWorkspace {
                store,
                known_data_version,
            }],
            notices: Vec::new(),
        }
    }

    pub fn on_refresh_completed(
        &mut self,
        store: WorkspaceStore,
        snapshot: Result<Option<WorkspaceSnapshot>, String>,
        live_ids: &HashSet<SessionId>,
    ) -> WorkspaceTransition {
        let access = match &self.io {
            WorkspaceIo::InFlight {
                access: StoreAccess::ReadOnly,
                ..
            } => StoreAccess::ReadOnly,
            _ => access_for(&store),
        };
        let mut notices = Vec::new();
        if let Some(snapshot) = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                notices.push(WorkspaceNotice::RefreshFailed { error });
                None
            }
        } {
            self.publish_snapshot(access, snapshot, live_ids);
        }
        self.io = WorkspaceIo::Ready { store, access };
        WorkspaceTransition {
            effects: Vec::new(),
            notices,
        }
    }

    pub fn on_refresh_task_lost(&mut self, db_path: PathBuf, error: String) -> WorkspaceTransition {
        let access = match &self.io {
            WorkspaceIo::InFlight { access, .. } => Some(*access),
            _ => None,
        };
        self.io = WorkspaceIo::Opening {
            db_path: db_path.clone(),
            attempt: OpenAttempt::Initial,
            access,
        };
        WorkspaceTransition {
            effects: vec![Effect::LoadWorkspaceSnapshot { db_path }],
            notices: vec![WorkspaceNotice::RefreshFailed { error }],
        }
    }

    pub fn on_write_task_lost(&mut self, db_path: PathBuf, error: String) -> WorkspaceTransition {
        self.io = WorkspaceIo::Opening {
            db_path: db_path.clone(),
            attempt: OpenAttempt::Initial,
            access: Some(StoreAccess::ReadWrite),
        };
        self.dirty = true;
        self.sync_pending = true;
        WorkspaceTransition {
            effects: vec![Effect::LoadWorkspaceSnapshot { db_path }],
            notices: vec![WorkspaceNotice::WriterFailed { error }],
        }
    }

    fn validate_layout_keys(&self, keys: &[MemberKey]) -> Result<(), LayoutRequestError> {
        if self.writes_disabled() {
            return Err(LayoutRequestError::ReadOnly);
        }
        let Some(view) = self.view() else {
            return Err(LayoutRequestError::MemberNotFound);
        };
        let present = view.members.iter().map(member_key).collect::<HashSet<_>>();
        if keys
            .iter()
            .all(|key| matches!(key.kind, MemberKind::Build) && present.contains(key))
        {
            Ok(())
        } else {
            Err(LayoutRequestError::MemberNotFound)
        }
    }

    fn has_pending_removals(&self) -> bool {
        self.removals
            .values()
            .any(|state| matches!(state, RemovalState::Pending { .. }))
    }

    fn pending_removal_keys(&self) -> Vec<MemberKey> {
        self.removals
            .iter()
            .filter_map(|(session_id, state)| {
                matches!(state, RemovalState::Pending { .. }).then_some(MemberKey {
                    session_id: session_id.clone(),
                    kind: MemberKind::Build,
                })
            })
            .collect()
    }

    fn pending_removal_key_set(&self) -> HashSet<MemberKey> {
        self.pending_removal_keys().into_iter().collect()
    }

    fn finish_removal(&mut self, session_id: &SessionId) {
        if matches!(
            self.removals.remove(session_id),
            Some(RemovalState::Pending {
                completion: RemovalCompletion::SuppressUntilExplicitLoad,
                ..
            })
        ) {
            self.removals.insert(
                session_id.clone(),
                RemovalState::SuppressedUntilExplicitLoad {
                    cause: SuppressionCause::HistoryDeleted,
                },
            );
        }
    }

    fn plan_upserts(&self, candidates: Vec<NewMember>) -> Vec<NewMember> {
        let Some(snapshot) = self.committed_snapshot.as_ref() else {
            return Vec::new();
        };
        let candidates = candidates
            .into_iter()
            .filter(|candidate| !self.removals.contains_key(&candidate.key.session_id))
            .collect::<Vec<_>>();
        let existing_ids: HashSet<_> = snapshot
            .members
            .iter()
            .filter(|member| matches!(member.kind, MemberKind::Build))
            .map(|member| member.session_id.clone())
            .collect();
        let candidate_ids: HashSet<_> = candidates
            .iter()
            .map(|candidate| candidate.key.session_id.clone())
            .collect();
        let pinned_non_candidates = snapshot
            .members
            .iter()
            .filter(|member| {
                member.pin_rank.is_some()
                    && !(matches!(member.kind, MemberKind::Build)
                        && candidate_ids.contains(&member.session_id))
            })
            .count();
        let (mut existing, missing): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .partition(|candidate| existing_ids.contains(&candidate.key.session_id));
        let missing_slots = WORKSPACE_CAPACITY
            .saturating_sub(pinned_non_candidates)
            .saturating_sub(existing.len());
        existing.extend(missing.into_iter().take(missing_slots));
        existing
            .into_iter()
            .filter(|candidate| {
                let stored = snapshot.members.iter().find(|member| {
                    matches!(member.kind, MemberKind::Build)
                        && member.session_id == candidate.key.session_id
                });
                if stored.is_some_and(|member| metadata_matches(member, &candidate.metadata)) {
                    return false;
                }
                !matches!(
                    self.upserts.get(&candidate.key.session_id),
                    Some(UpsertAttempt::Failed(metadata)) if metadata == &candidate.metadata
                )
            })
            .collect()
    }

    fn complete_upserts(
        &mut self,
        members: Vec<NewMember>,
        failures: &[WorkspaceMutationFailure],
    ) -> WorkspaceTransition {
        let mut retry = false;
        for member in members {
            let failure = failures.iter().find(|failure| failure.key == member.key);
            let Some(failure) = failure else {
                self.upserts.remove(&member.key.session_id);
                continue;
            };
            let already_retried = matches!(
                self.upserts.get(&member.key.session_id),
                Some(UpsertAttempt::Retried(metadata)) if metadata == &member.metadata
            );
            if failure.retryable && !already_retried {
                self.upserts.insert(
                    member.key.session_id,
                    UpsertAttempt::Retried(member.metadata),
                );
                retry = true;
            } else {
                self.upserts.insert(
                    member.key.session_id,
                    UpsertAttempt::Failed(member.metadata),
                );
            }
        }
        self.sync_pending |= retry;
        self.dirty |= retry;
        let notices = failures.first().map_or_else(Vec::new, |failure| {
            vec![WorkspaceNotice::SyncFailed {
                count: failures.len(),
                session_id: failure.key.session_id.to_string(),
                error: failure.error.clone(),
            }]
        });
        WorkspaceTransition {
            effects: Vec::new(),
            notices,
        }
    }

    fn complete_removals(
        &mut self,
        keys: Vec<MemberKey>,
        failures: &[WorkspaceMutationFailure],
    ) -> WorkspaceTransition {
        let mut retry = false;
        let mut terminal = Vec::new();
        for key in keys {
            let failure = failures.iter().find(|failure| failure.key == key);
            let Some(failure) = failure else {
                self.layout.prune_key(&key);
                self.finish_removal(&key.session_id);
                continue;
            };
            let first_retry = matches!(
                self.removals.get(&key.session_id),
                Some(RemovalState::Pending {
                    retry: RetryBudget::Fresh,
                    ..
                })
            );
            if failure.retryable && first_retry {
                if let Some(RemovalState::Pending { retry: budget, .. }) =
                    self.removals.get_mut(&key.session_id)
                {
                    *budget = RetryBudget::Retried;
                }
                retry = true;
            } else {
                self.finish_removal(&key.session_id);
                terminal.push(failure);
            }
        }
        let pending_removals = self.has_pending_removals();
        self.sync_pending |= failures.is_empty();
        self.dirty |= retry || pending_removals || failures.is_empty();
        let notices = terminal.first().map_or_else(Vec::new, |failure| {
            vec![WorkspaceNotice::ArchiveFailed {
                count: terminal.len(),
                session_id: failure.key.session_id.to_string(),
                error: failure.error.clone(),
            }]
        });
        WorkspaceTransition {
            effects: Vec::new(),
            notices,
        }
    }

    fn publish_snapshot(
        &mut self,
        access: StoreAccess,
        snapshot: WorkspaceSnapshot,
        live_ids: &HashSet<SessionId>,
    ) -> bool {
        let current_ids = build_member_ids(&snapshot);
        for session_id in &current_ids {
            if matches!(
                self.removals.get(session_id),
                Some(RemovalState::SuppressedUntilExplicitLoad {
                    cause: SuppressionCause::SnapshotRemoval,
                })
            ) {
                self.removals.remove(session_id);
            }
            self.explicit_adoptions.remove(session_id);
        }
        let previous_ids = self
            .committed_snapshot
            .as_ref()
            .map(build_member_ids)
            .unwrap_or_default();
        for session_id in previous_ids.difference(&current_ids) {
            if live_ids.contains(session_id)
                && !self.explicit_adoptions.contains(session_id)
                && !self.removals.contains_key(session_id)
            {
                self.removals.insert(
                    session_id.clone(),
                    RemovalState::SuppressedUntilExplicitLoad {
                        cause: SuppressionCause::SnapshotRemoval,
                    },
                );
            }
        }
        if access == StoreAccess::ReadOnly {
            let rejected_archive = self.has_pending_removals();
            self.removals.retain(|_, state| {
                matches!(state, RemovalState::SuppressedUntilExplicitLoad { .. })
            });
            self.upserts.clear();
            self.layout.clear();
            self.dirty = false;
            self.sync_pending = false;
            self.committed_snapshot = Some(snapshot);
            return rejected_archive;
        }

        self.layout.rebase_missing_members(&snapshot);
        self.committed_snapshot = Some(snapshot);
        false
    }
}

fn build_member_ids(snapshot: &WorkspaceSnapshot) -> HashSet<SessionId> {
    snapshot
        .members
        .iter()
        .filter(|member| matches!(member.kind, MemberKind::Build))
        .map(|member| member.session_id.clone())
        .collect()
}

fn access_for(store: &WorkspaceStore) -> StoreAccess {
    if matches!(store.schema_state(), SchemaState::Current) {
        StoreAccess::ReadWrite
    } else {
        StoreAccess::ReadOnly
    }
}

fn metadata_matches(member: &distill_dashboard_store::Member, metadata: &MemberMetadata) -> bool {
    member.cwd == metadata.cwd
        && member.title == metadata.title
        && member.model == metadata.model
        && member.last_turn_summary == metadata.last_turn_summary
        && member.is_worktree == metadata.is_worktree
        && member.last_change_unix_ms == metadata.last_change_unix_ms
}

#[cfg(test)]
#[path = "workspace_membership_tests.rs"]
mod tests;
