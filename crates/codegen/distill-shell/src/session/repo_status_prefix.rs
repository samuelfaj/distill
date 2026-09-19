// Modified for Distill by Samuel Fajreldines, 2026.
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use distill_workspace::session::git::VcsKind;

/// Bounds a cold status so a wedged VCS can't stall the prompt.
pub(crate) const REPO_STATUS_GATHER_TIMEOUT: Duration = Duration::from_secs(5);

/// Slack over the gather budget so a gather finishing at the deadline still lands.
pub(crate) const REPO_STATUS_WAIT_BUDGET: Duration =
    REPO_STATUS_GATHER_TIMEOUT.saturating_add(Duration::from_secs(1));

/// A handful of parallel sessions can prefetch at once; the rest gather inline.
const REPO_STATUS_PREFETCH_SLOTS_MAX: usize = 4;
static REPO_STATUS_PREFETCH_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(REPO_STATUS_PREFETCH_SLOTS_MAX)));

#[derive(Clone, Debug)]
pub(crate) struct RepoStatusInputs {
    pub(crate) cwd: PathBuf,
    pub(crate) vcs_kind: VcsKind,
    pub(crate) root: Option<PathBuf>,
}

pub(crate) fn discover_vcs_root(cwd: &std::path::Path) -> Option<PathBuf> {
    use distill_workspace::session::git::{GitDiscoveryResult, discover_git_root};
    match discover_git_root(cwd) {
        GitDiscoveryResult::Found(r) => {
            Some(PathBuf::from(r.to_string_lossy().trim_end_matches('/')))
        }
        _ => None,
    }
}

impl RepoStatusInputs {
    /// Discovers the repo root synchronously; the `core.fsmonitor` probe and the
    /// status gather run later (off the spawn path, in the task or inline).
    pub(crate) fn new(cwd: PathBuf, vcs_kind: VcsKind) -> Self {
        let root = discover_vcs_root(&cwd);
        Self {
            cwd,
            vcs_kind,
            root,
        }
    }

    pub(crate) fn missed_snapshot(&self) -> RepoStatusSnapshot {
        RepoStatusSnapshot::root_only(self.root.clone(), self.vcs_kind)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RepoStatusSnapshot {
    pub(crate) root: Option<PathBuf>,
    /// `None` = repo present, no body: the gather failed/timed out, or the status
    /// was suppressed (toggle off / non-interactive). The root is still surfaced.
    pub(crate) raw_status: Option<String>,
    pub(crate) vcs_kind: VcsKind,
}

impl RepoStatusSnapshot {
    /// Repo present with no status body (suppressed, failed, or timed out); the
    /// root is still surfaced.
    pub(crate) fn root_only(root: Option<PathBuf>, vcs_kind: VcsKind) -> Self {
        Self {
            root,
            raw_status: None,
            vcs_kind,
        }
    }

    pub(crate) fn templated_status(&self) -> Option<String> {
        self.raw_status
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim_end().to_string())
    }

    pub(crate) fn legacy_status(&self) -> Option<String> {
        let raw_status = self
            .raw_status
            .as_deref()
            .filter(|s| !s.trim().is_empty())?;
        match self.vcs_kind {
            VcsKind::JujutsuColocated => Some(raw_status.to_string()),
            VcsKind::Git => distill_agent::prompt::user_message::normalize_git_status(raw_status),
            VcsKind::None => None,
        }
    }
}

pub(crate) async fn gather_repo_status(inputs: &RepoStatusInputs) -> Option<RepoStatusSnapshot> {
    use distill_workspace::file_system::{
        git_status_short_pinned, jj_status, probe_fsmonitor_override,
    };

    let mut timer = crate::instrumentation_timer!("session.user_prefix.vcs_status");
    timer.with_field("timeout_ms", REPO_STATUS_GATHER_TIMEOUT.as_millis() as u64);

    let status = match inputs.vcs_kind {
        VcsKind::JujutsuColocated => {
            timer.with_field("vcs", "jj");
            timer.with_field("status_mode", "jj");
            tokio::time::timeout(REPO_STATUS_GATHER_TIMEOUT, jj_status(inputs.cwd.clone())).await
        }
        VcsKind::Git => {
            timer.with_field("vcs", "git");
            timer.with_field("status_mode", "short_untracked_normal");
            let cwd = inputs.cwd.clone();
            tokio::time::timeout(REPO_STATUS_GATHER_TIMEOUT, async move {
                let fsmonitor = probe_fsmonitor_override(&cwd).await;
                git_status_short_pinned(cwd, fsmonitor).await
            })
            .await
        }
        VcsKind::None => return None,
    };
    match status {
        Ok(Ok(raw_status)) => {
            timer.with_field("outcome", "success");
            timer.with_field("output_bytes", raw_status.len() as u64);
            Some(RepoStatusSnapshot {
                root: inputs.root.clone(),
                raw_status: Some(raw_status),
                vcs_kind: inputs.vcs_kind,
            })
        }
        Ok(Err(e)) => {
            timer.with_field("outcome", "error");
            timer.with_field("output_bytes", 0_u64);
            tracing::warn!("repo status gather failed: {e}");
            None
        }
        Err(_) => {
            timer.with_field("outcome", "timeout");
            timer.with_field("output_bytes", 0_u64);
            tracing::warn!(vcs = ?inputs.vcs_kind, "repo status gather timed out");
            None
        }
    }
}

pub(crate) struct RepoStatusPrefetch {
    snapshot_rx: watch::Receiver<Option<RepoStatusSnapshot>>,
    gather_task: JoinHandle<()>,
    cancel: CancellationToken,
}

impl RepoStatusPrefetch {
    /// Must run inside the session `LocalSet` (uses `spawn_local`). The permit is
    /// moved into the task so it frees when the gather finishes, not when the
    /// handle is consumed.
    pub(crate) fn spawn(inputs: RepoStatusInputs) -> Option<Self> {
        use tracing::Instrument;

        let Ok(permit) = REPO_STATUS_PREFETCH_SLOTS.clone().try_acquire_owned() else {
            return None;
        };

        let gather_span = tracing::info_span!("session.repo_status_gather");
        let (snapshot_tx, snapshot_rx) = watch::channel(None);
        let cancel = CancellationToken::new();
        let gather_cancel = cancel.clone();
        let gather_task = tokio::task::spawn_local(
            async move {
                let _permit = permit;
                let snapshot = tokio::select! {
                    biased;
                    _ = gather_cancel.cancelled() => return,
                    s = gather_repo_status(&inputs) => s,
                };
                let _ =
                    snapshot_tx.send(Some(snapshot.unwrap_or_else(|| inputs.missed_snapshot())));
            }
            .instrument(gather_span),
        );
        Some(Self {
            snapshot_rx,
            gather_task,
            cancel,
        })
    }

    pub(crate) async fn snapshot_within(
        &mut self,
        budget: std::time::Duration,
    ) -> Option<RepoStatusSnapshot> {
        match tokio::time::timeout(budget, self.snapshot_rx.wait_for(Option::is_some)).await {
            Ok(Ok(resolved)) => resolved.clone(),
            Ok(Err(_)) | Err(_) => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_snapshot_rx(
        snapshot_rx: watch::Receiver<Option<RepoStatusSnapshot>>,
    ) -> Self {
        Self {
            snapshot_rx,
            gather_task: tokio::spawn(async {}),
            cancel: CancellationToken::new(),
        }
    }
}

impl Drop for RepoStatusPrefetch {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.gather_task.abort();
    }
}

/// An enum so illegal combinations (a prefetch without a gather, a suppressed status with a live prefetch) are unrepresentable.
/// `RootOnly` = repo present, status body suppressed (toggle off / non-interactive): the root is still surfaced, nothing is gathered.
#[derive(Default)]
pub(crate) enum RepoStatusPlan {
    #[default]
    NoRepo,
    RootOnly {
        root: Option<PathBuf>,
        vcs_kind: VcsKind,
    },
    Gather {
        inputs: RepoStatusInputs,
        prefetch: RefCell<Option<RepoStatusPrefetch>>,
    },
}

#[derive(Default)]
pub(crate) struct RepoStatusPrefetchState {
    // Boxed to keep `SessionActor` small (it is near the debug stack-size limit).
    plan: Box<RepoStatusPlan>,
    wait_ms: Cell<Option<u64>>,
}

impl RepoStatusPrefetchState {
    pub(crate) fn new(plan: RepoStatusPlan) -> Self {
        Self {
            plan: Box::new(plan),
            wait_ms: Cell::new(None),
        }
    }

    pub(crate) fn plan(&self) -> &RepoStatusPlan {
        &self.plan
    }

    /// Consumes the pending prefetch handle. `None` for a non-`Gather` plan or
    /// once it has already been taken.
    pub(crate) fn take_prefetch(&self) -> Option<RepoStatusPrefetch> {
        match self.plan() {
            RepoStatusPlan::Gather { prefetch, .. } => prefetch.borrow_mut().take(),
            _ => None,
        }
    }

    pub(crate) fn record_wait(&self, waited: std::time::Duration) -> u64 {
        let ms = waited.as_millis() as u64;
        self.wait_ms.set(Some(ms));
        ms
    }

    pub(crate) fn take_wait_ms(&self) -> Option<u64> {
        self.wait_ms.take()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn repo_status_prefetch_resolved_before_prompt_is_used_without_waiting() {
        let (snapshot_tx, snapshot_rx) = watch::channel(None);
        snapshot_tx
            .send(Some(RepoStatusSnapshot {
                root: Some(PathBuf::from("/repo")),
                raw_status: Some("## main\n M src/lib.rs\n".to_string()),
                vcs_kind: VcsKind::Git,
            }))
            .unwrap();
        let mut prefetch = RepoStatusPrefetch::from_snapshot_rx(snapshot_rx);

        let start = tokio::time::Instant::now();
        let snapshot = prefetch
            .snapshot_within(REPO_STATUS_WAIT_BUDGET)
            .await
            .expect("resolved prefetch must yield its snapshot");
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
        assert_eq!(snapshot.root.as_deref(), Some(Path::new("/repo")));
        assert_eq!(
            snapshot.templated_status().as_deref(),
            Some("## main\n M src/lib.rs")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn repo_status_prefetch_unresolved_past_budget_returns_none() {
        let (_snapshot_tx, snapshot_rx) = watch::channel::<Option<RepoStatusSnapshot>>(None);
        let mut prefetch = RepoStatusPrefetch::from_snapshot_rx(snapshot_rx);

        let start = tokio::time::Instant::now();
        assert!(
            prefetch
                .snapshot_within(REPO_STATUS_WAIT_BUDGET)
                .await
                .is_none(),
            "budget miss must omit the status, not block on the gather"
        );
        assert_eq!(start.elapsed(), REPO_STATUS_WAIT_BUDGET);
    }

    #[tokio::test(start_paused = true)]
    async fn repo_status_prefetch_resolving_within_budget_keeps_the_block() {
        let (snapshot_tx, snapshot_rx) = watch::channel(None);
        let mut prefetch = RepoStatusPrefetch::from_snapshot_rx(snapshot_rx);
        let gather = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
            let _ = snapshot_tx.send(Some(RepoStatusSnapshot {
                root: None,
                raw_status: Some("## main\n M src/lib.rs\n".to_string()),
                vcs_kind: VcsKind::Git,
            }));
        });

        let snapshot = prefetch
            .snapshot_within(REPO_STATUS_WAIT_BUDGET)
            .await
            .expect("a gather resolving within the budget must keep its status");
        assert_eq!(
            snapshot.templated_status().as_deref(),
            Some("## main\n M src/lib.rs")
        );
        gather.await.unwrap();
    }

    #[test]
    fn snapshot_status_shaping_matches_the_inline_gather_paths() {
        let git = RepoStatusSnapshot {
            root: None,
            raw_status: Some(" M src/main.rs\n?? new.txt\n".to_string()),
            vcs_kind: VcsKind::Git,
        };
        assert_eq!(
            git.templated_status().as_deref(),
            Some(" M src/main.rs\n?? new.txt")
        );
        assert_eq!(
            git.legacy_status(),
            distill_agent::prompt::user_message::normalize_git_status(
                " M src/main.rs\n?? new.txt\n"
            )
        );

        let empty = RepoStatusSnapshot {
            root: None,
            raw_status: Some("   \n".to_string()),
            vcs_kind: VcsKind::Git,
        };
        assert!(empty.templated_status().is_none());
        assert!(empty.legacy_status().is_none());

        let jj = RepoStatusSnapshot {
            root: None,
            raw_status: Some("Working copy changes:\nM src/lib.rs\n".to_string()),
            vcs_kind: VcsKind::JujutsuColocated,
        };
        assert_eq!(
            jj.legacy_status().as_deref(),
            Some("Working copy changes:\nM src/lib.rs\n")
        );

        let jj_empty = RepoStatusSnapshot {
            root: None,
            raw_status: Some("  \n".to_string()),
            vcs_kind: VcsKind::JujutsuColocated,
        };
        assert!(jj_empty.templated_status().is_none());
        assert!(jj_empty.legacy_status().is_none());

        let missing = RepoStatusSnapshot {
            root: None,
            raw_status: None,
            vcs_kind: VcsKind::Git,
        };
        assert!(missing.templated_status().is_none());
        assert!(missing.legacy_status().is_none());
    }

    #[test]
    fn recorded_wait_drains_exactly_once() {
        let state = RepoStatusPrefetchState::default();
        assert_eq!(state.take_wait_ms(), None);
        assert_eq!(state.record_wait(std::time::Duration::from_millis(42)), 42);
        assert_eq!(state.take_wait_ms(), Some(42));
        assert_eq!(state.take_wait_ms(), None);
    }

    #[test]
    fn missed_snapshot_carries_the_root_without_a_status_body() {
        let inputs = RepoStatusInputs {
            cwd: PathBuf::from("/repo"),
            vcs_kind: VcsKind::Git,
            root: Some(PathBuf::from("/repo")),
        };
        let missed = inputs.missed_snapshot();
        assert_eq!(missed.root.as_deref(), Some(Path::new("/repo")));
        assert!(missed.raw_status.is_none());
        assert!(missed.templated_status().is_none());
        assert!(missed.legacy_status().is_none());
    }
}
