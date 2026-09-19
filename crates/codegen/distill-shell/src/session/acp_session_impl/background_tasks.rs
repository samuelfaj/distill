// Modified for Distill by Samuel Fajreldines, 2026.
//! Focused `SessionActor` helpers for durable `background_tasks` snapshots.

use std::sync::atomic::Ordering;
use std::time::Duration;

use super::*;
use crate::extensions::background_task::{
    SnapshotListOutcome, background_tasks_update, snapshot_list_outcome,
};

/// Overall budget for snapshot `list_tasks_metadata` on the session actor.
/// Local ACP enumeration is sync, but other backends still go through an await;
/// a hung client must not stall every later `SessionCommand`.
pub(crate) const BACKGROUND_TASKS_LIST_BUDGET: Duration = Duration::from_secs(2);

impl SessionActor {
    /// List (metadata-only), fit, persist, and broadcast a last-wins
    /// `SessionUpdate::BackgroundTasks` snapshot.
    ///
    /// Timeout skips the emit (does not persist `tasks: []`). Missing backend
    /// still clears. Coalescing relies on a queued follow-up command rather than
    /// an in-arm retry loop.
    pub(super) async fn emit_background_tasks_snapshot(
        &self,
        pending: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) {
        // Always clear coalescing pending so a skipped emit cannot stick true.
        if let Some(flag) = &pending {
            flag.store(false, Ordering::Release);
        }

        if !self.emit_local_background_tasks.load(Ordering::Acquire) {
            tracing::debug!(
                session_id = %self.session_info.id,
                "skipping local background_tasks snapshot emit (gateway-backed)"
            );
            return;
        }

        let list_result = tokio::time::timeout(
            BACKGROUND_TASKS_LIST_BUDGET,
            self.tool_bridge_handle().list_tasks_metadata(),
        )
        .await
        .map_err(|_| ());

        // Re-check after the list await: a mid-session gateway bind can flip the
        // flag off while we were listing; broadcasting then would last-wins-clear
        // remote Running — the case the flag was added to prevent.
        if !self.emit_local_background_tasks.load(Ordering::Acquire) {
            tracing::debug!(
                session_id = %self.session_info.id,
                "skipping local background_tasks snapshot emit after list (gateway-backed)"
            );
            return;
        }

        match snapshot_list_outcome(list_result) {
            SnapshotListOutcome::SkipTimeout => {
                tracing::warn!(
                    session_id = %self.session_info.id,
                    budget_ms = BACKGROUND_TASKS_LIST_BUDGET.as_millis() as u64,
                    "background_tasks list_tasks exceeded budget; skipping snapshot emit"
                );
            }
            SnapshotListOutcome::ClearMissingBackend => {
                tracing::warn!(
                    session_id = %self.session_info.id,
                    "emitting empty background_tasks snapshot: no terminal backend"
                );
                self.broadcast_background_tasks_update(std::iter::empty())
                    .await;
            }
            SnapshotListOutcome::Tasks(snapshots) => {
                self.broadcast_background_tasks_update(snapshots).await;
            }
        }
    }

    async fn broadcast_background_tasks_update(
        &self,
        snapshots: impl IntoIterator<Item = distill_tools::types::TaskSnapshot>,
    ) {
        let Some(update) = background_tasks_update(
            snapshots,
            &self.session_info.id,
            Some(self.build_notification_meta()),
        ) else {
            tracing::warn!(
                session_id = %self.session_info.id,
                "skipping unfittable background_tasks snapshot"
            );
            return;
        };
        self.send_xai_notification(update).await;
    }
}
