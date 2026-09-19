const DEFAULT_FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

static LAST_FLUSH_TIMEOUT_SECS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(u64::MAX);

pub(crate) fn capture_flush_timeout_pre_exit(flush_timeout: std::time::Duration) {
    LAST_FLUSH_TIMEOUT_SECS.store(
        flush_timeout.as_secs(),
        std::sync::atomic::Ordering::Relaxed,
    );
}

pub(crate) fn upload_flush_timeout_pre_exit() -> std::time::Duration {
    match LAST_FLUSH_TIMEOUT_SECS.load(std::sync::atomic::Ordering::Relaxed) {
        u64::MAX => DEFAULT_FLUSH_TIMEOUT,
        secs => std::time::Duration::from_secs(secs),
    }
}

static PENDING_UPLOAD_TASKS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub(super) struct PendingUploadGuard;

impl PendingUploadGuard {
    #[must_use]
    pub(super) fn register() -> Self {
        PENDING_UPLOAD_TASKS.fetch_add(1, std::sync::atomic::Ordering::Release);
        Self
    }
}

impl Drop for PendingUploadGuard {
    fn drop(&mut self) {
        PENDING_UPLOAD_TASKS.fetch_sub(1, std::sync::atomic::Ordering::Release);
    }
}

pub async fn drain_pending_uploads(timeout: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    while PENDING_UPLOAD_TASKS.load(std::sync::atomic::Ordering::Acquire) > 0 {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(
                pending = PENDING_UPLOAD_TASKS.load(std::sync::atomic::Ordering::Acquire),
                "gave up draining pending trace uploads at exit"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// The floor overrides a smaller configured timeout so a slow-starting
/// prompt-metadata upload is not abandoned at exit.
pub async fn drain_pending_uploads_at_exit() {
    let drain_budget = upload_flush_timeout_pre_exit().max(exit_drain_min());
    drain_pending_uploads(drain_budget).await;
}

/// Floor for the exit drain: the longest a turn-end upload can defer its start
/// (`PARSED_PROMPT_WAIT`) plus one bounded attempt to reach the bucket.
fn exit_drain_min() -> std::time::Duration {
    crate::session::commands::PARSED_PROMPT_WAIT + super::trace::BLOCKING_ATTEMPT_CAP
}

#[cfg(test)]
mod tests {
    use super::*;

    static DRAIN_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[test]
    fn exit_drain_min_outlasts_parsed_prompt_wait() {
        assert!(
            exit_drain_min() > crate::session::commands::PARSED_PROMPT_WAIT,
            "exit drain floor {:?} must outlast PARSED_PROMPT_WAIT {:?}",
            exit_drain_min(),
            crate::session::commands::PARSED_PROMPT_WAIT,
        );
    }

    #[tokio::test]
    async fn panicking_upload_task_releases_pending_guard() {
        let _lock = DRAIN_TEST_LOCK.lock().await;
        let before = PENDING_UPLOAD_TASKS.load(std::sync::atomic::Ordering::Acquire);
        crate::upload::turn::spawn_upload_task("panicking_test", async {
            panic!("boom");
        });
        assert!(
            PENDING_UPLOAD_TASKS.load(std::sync::atomic::Ordering::Acquire) > before,
            "spawn must count the task synchronously"
        );
        drain_pending_uploads(std::time::Duration::from_secs(5)).await;
        assert_eq!(
            PENDING_UPLOAD_TASKS.load(std::sync::atomic::Ordering::Acquire),
            before,
            "a caught panic must return the pending counter to baseline"
        );
    }

    #[tokio::test]
    async fn drain_pending_uploads_is_budget_bounded() {
        let _lock = DRAIN_TEST_LOCK.lock().await;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        crate::upload::turn::spawn_upload_task("pending_test", async move {
            let _ = rx.await;
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            drain_pending_uploads(std::time::Duration::ZERO),
        )
        .await
        .expect("budget-bounded drain must return while a task is pending");
        drop(tx);
        drain_pending_uploads(std::time::Duration::from_secs(5)).await;
    }
}
