use std::time::Duration;

pub const OTEL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

pub fn run_with_timeout(what: &str, timeout: Duration, work: impl FnOnce() + Send + 'static) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        work();
        let _ = tx.send(());
    });
    if rx.recv_timeout(timeout).is_err() {
        tracing::debug!("{what}: shutdown exceeded {timeout:?}; continuing exit without it");
    }
}
