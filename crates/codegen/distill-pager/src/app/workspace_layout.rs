// Modified for Distill by Samuel Fajreldines, 2026.
//! Derives dashboard-v2 layout from immutable committed snapshots plus pending intent.

use std::collections::{HashMap, HashSet};

use distill_dashboard_store::{
    Grouping, LayoutApplyOutcome, LayoutGrouping, LayoutPatch, Member, MemberKey, MemberKind,
    PinAssignment, RANK_GAP, StoreError, WorkspaceSnapshot,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LayoutOverlay {
    pin_overrides: HashMap<MemberKey, bool>,
    manual_order: Option<Vec<MemberKey>>,
    grouping: Option<LayoutGrouping>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceView {
    pub grouping: WorkspaceGrouping,
    pub members: Vec<Member>,
    pub data_version: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceGrouping {
    State,
    Directory,
}

impl WorkspaceGrouping {
    pub fn toggled(self) -> Self {
        match self {
            Self::State => Self::Directory,
            Self::Directory => Self::State,
        }
    }
}

impl From<WorkspaceGrouping> for Grouping {
    fn from(grouping: WorkspaceGrouping) -> Self {
        match grouping {
            WorkspaceGrouping::State => Self::State,
            WorkspaceGrouping::Directory => Self::Directory,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct LayoutState {
    overlay: LayoutOverlay,
    busy_retried: bool,
}

#[derive(Debug)]
pub(crate) enum LayoutCompletion {
    Continue {
        snapshot: Option<WorkspaceSnapshot>,
        retry: bool,
        failure: Option<String>,
    },
    DisableWrites {
        snapshot: Option<WorkspaceSnapshot>,
    },
}

impl WorkspaceView {
    pub fn manual_order(&self) -> Vec<MemberKey> {
        manual_order_from_members(&self.members)
    }

    #[cfg(test)]
    pub(crate) fn from_snapshot(snapshot: &WorkspaceSnapshot) -> Self {
        LayoutOverlay::default().view(snapshot, &HashSet::new())
    }
}

impl LayoutState {
    pub fn is_empty(&self) -> bool {
        self.overlay.is_empty()
    }

    pub fn set_pinned(&mut self, key: MemberKey, pinned: bool) {
        self.overlay.set_pinned(key, pinned);
    }

    pub fn set_manual_order(&mut self, keys: Vec<MemberKey>) {
        self.overlay.set_manual_order(keys);
    }

    pub fn set_grouping(&mut self, grouping: WorkspaceGrouping) {
        self.overlay.set_grouping(match grouping {
            WorkspaceGrouping::State => LayoutGrouping::State,
            WorkspaceGrouping::Directory => LayoutGrouping::Directory,
        });
    }

    pub fn prune_key(&mut self, key: &MemberKey) {
        self.overlay.prune_key(key);
        self.reset_retry_if_empty();
    }

    pub fn clear(&mut self) {
        self.overlay.clear();
        self.busy_retried = false;
    }

    pub fn view(
        &self,
        snapshot: &WorkspaceSnapshot,
        pending_removals: &HashSet<MemberKey>,
    ) -> WorkspaceView {
        self.overlay.view(snapshot, pending_removals)
    }

    pub fn plan(
        &self,
        snapshot: &WorkspaceSnapshot,
        pending_removals: &HashSet<MemberKey>,
    ) -> LayoutPatch {
        self.overlay.plan(snapshot, pending_removals)
    }

    pub fn reconcile_committed(&mut self, snapshot: &WorkspaceSnapshot) {
        self.overlay.reconcile_committed(snapshot);
        self.reset_retry_if_empty();
    }

    pub fn rebase_missing_members(&mut self, snapshot: &WorkspaceSnapshot) {
        self.overlay.rebase_missing_members(snapshot);
        self.reset_retry_if_empty();
    }

    pub fn complete(
        &mut self,
        dispatched: &LayoutPatch,
        outcome: LayoutApplyOutcome,
    ) -> LayoutCompletion {
        match outcome {
            LayoutApplyOutcome::Committed(snapshot) => {
                self.overlay.acknowledge(dispatched);
                self.busy_retried = false;
                LayoutCompletion::Continue {
                    snapshot: Some(snapshot),
                    retry: !self.overlay.is_empty(),
                    failure: None,
                }
            }
            LayoutApplyOutcome::Rejected { error, snapshot } => {
                self.complete_error(dispatched, &error, Some(snapshot))
            }
            LayoutApplyOutcome::Failed { error } => self.complete_error(dispatched, &error, None),
        }
    }

    fn complete_error(
        &mut self,
        dispatched: &LayoutPatch,
        error: &StoreError,
        snapshot: Option<WorkspaceSnapshot>,
    ) -> LayoutCompletion {
        if is_fatal(error) {
            self.clear();
            return LayoutCompletion::DisableWrites { snapshot };
        }
        if matches!(error, StoreError::Busy { .. }) && !self.busy_retried {
            self.busy_retried = true;
            return LayoutCompletion::Continue {
                snapshot,
                retry: true,
                failure: None,
            };
        }

        let member_not_found = matches!(error, StoreError::MemberNotFound { .. });
        if member_not_found {
            if let Some(snapshot) = snapshot.as_ref() {
                self.overlay.rebase_missing_members(snapshot);
            } else {
                self.overlay.acknowledge(dispatched);
            }
        } else {
            self.overlay.acknowledge(dispatched);
        }
        self.busy_retried = false;
        let retry = !self.overlay.is_empty();
        LayoutCompletion::Continue {
            snapshot,
            retry,
            failure: (!member_not_found || !retry).then(|| error.to_string()),
        }
    }

    fn reset_retry_if_empty(&mut self) {
        if self.overlay.is_empty() {
            self.busy_retried = false;
        }
    }
}

impl LayoutOverlay {
    pub fn is_empty(&self) -> bool {
        self.pin_overrides.is_empty() && self.manual_order.is_none() && self.grouping.is_none()
    }

    pub fn set_pinned(&mut self, key: MemberKey, pinned: bool) {
        self.pin_overrides.insert(key, pinned);
    }

    pub fn set_manual_order(&mut self, keys: Vec<MemberKey>) {
        self.manual_order = Some(deduplicated_build_keys(keys));
    }

    pub fn set_grouping(&mut self, grouping: LayoutGrouping) {
        self.grouping = Some(grouping);
    }

    pub fn prune_key(&mut self, key: &MemberKey) {
        self.pin_overrides.remove(key);
        if let Some(order) = self.manual_order.as_mut() {
            order.retain(|candidate| candidate != key);
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn view(
        &self,
        snapshot: &WorkspaceSnapshot,
        pending_removals: &HashSet<MemberKey>,
    ) -> WorkspaceView {
        let manual_ranks = self.manual_order.as_ref().map(|order| {
            order
                .iter()
                .enumerate()
                .map(|(index, key)| (key, rank_for_index(index)))
                .collect::<HashMap<_, _>>()
        });
        let members = snapshot
            .members
            .iter()
            .filter(|member| !pending_removals.contains(&member_key(member)))
            .cloned()
            .map(|mut member| {
                let key = member_key(&member);
                if let Some(pinned) = self.pin_overrides.get(&key) {
                    member.pin_rank = pinned.then_some(RANK_GAP);
                }
                if matches!(member.kind, MemberKind::Build)
                    && let Some(manual_ranks) = &manual_ranks
                {
                    member.order_rank = manual_ranks.get(&key).copied();
                }
                member
            })
            .collect();
        WorkspaceView {
            grouping: self.grouping.map_or_else(
                || render_grouping(&snapshot.grouping),
                |grouping| match grouping {
                    LayoutGrouping::State => WorkspaceGrouping::State,
                    LayoutGrouping::Directory => WorkspaceGrouping::Directory,
                },
            ),
            members,
            data_version: snapshot.data_version,
        }
    }

    pub fn plan(
        &self,
        snapshot: &WorkspaceSnapshot,
        pending_removals: &HashSet<MemberKey>,
    ) -> LayoutPatch {
        let members = snapshot
            .members
            .iter()
            .filter(|member| matches!(member.kind, MemberKind::Build))
            .map(|member| (member_key(member), member))
            .collect::<HashMap<_, _>>();
        let mut pin_assignments = self
            .pin_overrides
            .iter()
            .filter(|(key, _)| !pending_removals.contains(*key))
            .filter_map(|(key, pinned)| {
                let member = members.get(key)?;
                (member.pin_rank.is_some() != *pinned).then(|| PinAssignment {
                    key: key.clone(),
                    pinned: *pinned,
                })
            })
            .collect::<Vec<_>>();
        pin_assignments.sort_by(|left, right| {
            left.key
                .session_id
                .as_ref()
                .cmp(right.key.session_id.as_ref())
                .then_with(|| left.key.kind.as_str().cmp(right.key.kind.as_str()))
        });
        let manual_order = self.manual_order.as_ref().and_then(|order| {
            let valid = order
                .iter()
                .filter(|key| members.contains_key(*key) && !pending_removals.contains(*key))
                .cloned()
                .collect::<Vec<_>>();
            (stored_manual_order(snapshot) != valid).then_some(valid)
        });
        let grouping = self
            .grouping
            .filter(|grouping| snapshot.grouping.as_str() != grouping.as_ref());
        LayoutPatch {
            pin_assignments,
            manual_order,
            grouping,
        }
    }

    /// Preserves newer gestures on fields represented by the acknowledged patch.
    pub fn acknowledge(&mut self, dispatched: &LayoutPatch) {
        for assignment in &dispatched.pin_assignments {
            let dispatched_pinned = assignment.pinned;
            if self.pin_overrides.get(&assignment.key) == Some(&dispatched_pinned) {
                self.pin_overrides.remove(&assignment.key);
            }
        }
        if dispatched
            .manual_order
            .as_ref()
            .is_some_and(|order| self.manual_order.as_ref() == Some(order))
        {
            self.manual_order = None;
        }
        if dispatched
            .grouping
            .as_ref()
            .is_some_and(|grouping| self.grouping.as_ref() == Some(grouping))
        {
            self.grouping = None;
        }
    }

    pub fn rebase_missing_members(&mut self, snapshot: &WorkspaceSnapshot) {
        let present = snapshot
            .members
            .iter()
            .filter(|member| matches!(member.kind, MemberKind::Build))
            .map(member_key)
            .collect::<HashSet<_>>();
        self.pin_overrides.retain(|key, _| present.contains(key));
        if let Some(order) = self.manual_order.as_mut() {
            order.retain(|key| present.contains(key));
        }
    }

    pub fn reconcile_committed(&mut self, snapshot: &WorkspaceSnapshot) {
        self.pin_overrides.retain(|key, pinned| {
            snapshot
                .members
                .iter()
                .find(|member| member_key(member) == *key)
                .is_some_and(|member| member.pin_rank.is_some() != *pinned)
        });
        if self
            .manual_order
            .as_ref()
            .is_some_and(|order| stored_manual_order(snapshot) == *order)
        {
            self.manual_order = None;
        }
        if self
            .grouping
            .as_ref()
            .is_some_and(|grouping| snapshot.grouping.as_str() == grouping.as_ref())
        {
            self.grouping = None;
        }
    }
}

pub(crate) fn member_key(member: &Member) -> MemberKey {
    MemberKey {
        session_id: member.session_id.clone(),
        kind: member.kind.clone(),
    }
}

#[expect(
    clippy::expect_used,
    reason = "workspace capacity bounds every explicit layout rank"
)]
fn rank_for_index(index: usize) -> i64 {
    i64::try_from(index + 1)
        .ok()
        .and_then(|value| value.checked_mul(RANK_GAP))
        .expect("workspace capacity keeps layout ranks in range")
}

fn deduplicated_build_keys(keys: Vec<MemberKey>) -> Vec<MemberKey> {
    let mut seen = HashSet::with_capacity(keys.len());
    keys.into_iter()
        .filter(|key| matches!(key.kind, MemberKind::Build))
        .filter(|key| seen.insert(key.clone()))
        .collect()
}

fn stored_manual_order(snapshot: &WorkspaceSnapshot) -> Vec<MemberKey> {
    manual_order_from_members(&snapshot.members)
}

fn manual_order_from_members(members: &[Member]) -> Vec<MemberKey> {
    let mut ranked = members
        .iter()
        .filter(|member| matches!(member.kind, MemberKind::Build))
        .filter_map(|member| member.order_rank.map(|rank| (rank, member_key(member))))
        .collect::<Vec<_>>();
    ranked.sort_by(|(left_rank, left_key), (right_rank, right_key)| {
        left_rank.cmp(right_rank).then_with(|| {
            left_key
                .session_id
                .as_ref()
                .cmp(right_key.session_id.as_ref())
        })
    });
    ranked.into_iter().map(|(_, key)| key).collect()
}

fn render_grouping(grouping: &Grouping) -> WorkspaceGrouping {
    match grouping {
        Grouping::Directory => WorkspaceGrouping::Directory,
        Grouping::State | Grouping::Other(_) => WorkspaceGrouping::State,
    }
}

fn is_fatal(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::NewerSchema { .. } | StoreError::Unusable { .. }
    )
}

#[cfg(test)]
#[path = "workspace_layout_tests.rs"]
mod tests;
