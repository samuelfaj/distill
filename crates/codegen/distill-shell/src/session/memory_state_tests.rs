use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{CaptureWorker, FlushLockGuard, V2DreamWorkers, run_v2_initialization_blocking};

#[tokio::test(flavor = "current_thread")]
async fn v2_initialization_crosses_the_blocking_boundary() {
    let actor_thread = std::thread::current().id();
    let initialization_thread = run_v2_initialization_blocking(|| Ok(std::thread::current().id()))
        .await
        .unwrap();

    assert_ne!(
        initialization_thread, actor_thread,
        "v2 filesystem initialization must not run on the actor thread"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn v2_initialization_preserves_typed_storage_failures() {
    let error = run_v2_initialization_blocking(|| {
        Err::<(), _>(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        ))
    })
    .await
    .unwrap_err();

    assert!(matches!(
        error,
        super::MemoryInitializationError::Storage(ref source)
            if source.kind() == std::io::ErrorKind::PermissionDenied
    ));
}
#[tokio::test(flavor = "current_thread")]
async fn aborting_worker_drops_flush_guard() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let is_flushing = Arc::new(AtomicBool::new(false));
            let guard = FlushLockGuard::try_acquire(Arc::clone(&is_flushing)).unwrap();
            let task = tokio::task::spawn_local(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            });
            assert!(is_flushing.load(Ordering::Acquire));

            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(!is_flushing.load(Ordering::Acquire));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn capture_worker_cancels_cooperatively_and_waits_for_join() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let cancel = tokio_util::sync::CancellationToken::new();
            let task_cancel = cancel.clone();
            let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let task = tokio::task::spawn_local(async move {
                task_cancel.cancelled().await;
                let _ = cancelled_tx.send(());
                let _ = release_rx.await;
            });
            let worker = CaptureWorker::new(cancel, task);
            let stop = tokio::task::spawn_local(worker.cancel_and_join());

            cancelled_rx.await.unwrap();
            assert!(
                !stop.is_finished(),
                "capture teardown must retain and await the joinable worker"
            );
            release_tx.send(()).unwrap();
            stop.await.unwrap();
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_capture_worker_cancels_and_aborts_it() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let cancel = tokio_util::sync::CancellationToken::new();
            let observed = cancel.clone();
            let task = tokio::task::spawn_local(std::future::pending());
            let abort = task.abort_handle();
            let worker = CaptureWorker::new(cancel, task);

            drop(worker);
            tokio::task::yield_now().await;

            assert!(observed.is_cancelled());
            assert!(abort.is_finished());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn dream_workers_are_cancelled_and_joined_on_teardown() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let workers = V2DreamWorkers::default();
            let cancel = workers.cancellation_token();
            let joined = std::rc::Rc::new(std::cell::Cell::new(false));
            let joined_from_task = std::rc::Rc::clone(&joined);
            let task = tokio::task::spawn_local(async move {
                cancel.cancelled().await;
                tokio::task::yield_now().await;
                joined_from_task.set(true);
            });
            workers.track(task);

            workers.cancel_and_join().await;

            assert!(joined.get());
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_dream_workers_cancels_and_aborts_tasks() {
    tokio::task::LocalSet::new()
        .run_until(async {
            struct DropNotice(Option<tokio::sync::oneshot::Sender<()>>);

            impl Drop for DropNotice {
                fn drop(&mut self) {
                    if let Some(sender) = self.0.take() {
                        let _ = sender.send(());
                    }
                }
            }

            let workers = V2DreamWorkers::default();
            let cancel = workers.cancellation_token();
            let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let task = tokio::task::spawn_local(async move {
                let _notice = DropNotice(Some(dropped_tx));
                let _ = started_tx.send(());
                std::future::pending::<()>().await;
            });
            workers.track(task);
            started_rx.await.unwrap();

            drop(workers);

            assert!(cancel.is_cancelled());
            dropped_rx.await.unwrap();
        })
        .await;
}
